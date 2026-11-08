//! The benchmark tool (NFR-PERF-08): drives the release binary through
//! fixed scenarios and writes one CSV row per scenario and run with CPU
//! time and system calls per request; see README.md.
//!
//! Per run a fresh server; per scenario a warm-up, the measured window
//! (the server's utime + stime before and after, divided by the complete,
//! correct responses), then a separate strace pass for system calls. A
//! failure prints `bench failed: scenario=<s> reason=<r> (<detail>)`,
//! which `scripts/nightly-report.py` files, and the exit status is 1.

mod backend;
mod cli;
mod fds;
mod load;
mod measure;
mod netem;
mod row;
mod scenario;
mod server;
mod strace;

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use backend::Backend;
use load::Target;
use row::Row;
use scenario::{Routes, Scenario};
use server::{Place, Server};

/// Client threads per scenario.
const CONCURRENCY: usize = 8;
/// The longest warm-up.
pub(crate) const WARM_UP: Duration = Duration::from_secs(1);
/// Time for the server to finish the closes after the load, inside the
/// measured window.
pub(crate) const SETTLE: Duration = Duration::from_millis(100);

/// Why a scenario failed (the fingerprint's reason).
#[derive(Debug, Clone, Copy)]
enum Reason {
    Startup,
    Errors,
    NoRequests,
    Proc,
    Strace,
    ServerExited,
    Netem,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Reason::Startup => "startup",
            Reason::Errors => "errors",
            Reason::NoRequests => "no-requests",
            Reason::Proc => "proc",
            Reason::Strace => "strace",
            Reason::ServerExited => "server-exited",
            Reason::Netem => "netem",
        }
    }
}

fn fail(scenario: &str, reason: Reason, detail: &str) {
    println!(
        "bench failed: scenario={scenario} reason={} ({detail})",
        reason.name()
    );
}

fn write_csv(path: &Path, rows: &[Row]) -> std::io::Result<()> {
    let mut text = String::from(row::HEADER);
    text.push('\n');
    for r in rows {
        text.push_str(&r.line());
        text.push('\n');
    }
    fs::write(path, text)
}

struct Context {
    date: String,
    commit: String,
    runner: String,
    duration: Duration,
    run: u32,
    syscalls_only: bool,
    /// `--netem`'s namespace and pair, and `--pause`.
    net: Option<netem::Net>,
    pause: Option<Duration>,
}

/// One scenario on a running server: its row, or None after a failure
/// that left no numbers (printed).
fn measure(ctx: &Context, server: &mut Server, scenario: Scenario) -> Option<Row> {
    let name = scenario.name();
    let etag = match scenario {
        Scenario::ConditionalHit => {
            let probe = load::request(scenario::HOST, scenario.path(), "", load::Reuse::Fresh);
            load::header_of(server.addr, &probe, "etag")
        }
        Scenario::StaticSmall
        | Scenario::StaticLarge
        | Scenario::StaticFresh
        | Scenario::ProxySmall
        | Scenario::ProxyFresh
        | Scenario::StaticSmall1kIdle
        | Scenario::ProxyMedium
        | Scenario::ProxyLarge
        | Scenario::ProxyFile => None,
    };
    let etag = etag.as_deref();
    let target = Target {
        addr: server.addr,
        request: load::request(
            scenario::HOST,
            scenario.path(),
            &scenario.extra_headers(etag),
            scenario.reuse(),
        ),
        reuse: scenario.reuse(),
        expect: scenario.expect(etag),
    };
    // Held open through the measured window and the strace pass.
    let idle = match load::hold_idle(&target, scenario.idle()) {
        Ok(idle) => idle,
        Err(e) => {
            fail(name, Reason::Errors, &format!("idle connections: {e}"));
            return None;
        }
    };
    load::drive(&target, CONCURRENCY, ctx.duration.min(WARM_UP));

    let (mut tally, seconds, cpu, rss) = if ctx.syscalls_only {
        (
            load::Tally::default(),
            ctx.duration.as_secs_f64(),
            None,
            None,
        )
    } else {
        let before = server.cpu_ticks();
        let started = Instant::now();
        let (tally, paused) = thread::scope(|scope| {
            let pause = match (&ctx.net, ctx.pause) {
                (Some(net), Some(pause)) => Some(scope.spawn(move || {
                    thread::sleep(ctx.duration / 3);
                    let paused = Instant::now();
                    net.pause(pause).map(|()| (paused, Instant::now()))
                })),
                _ => None,
            };
            let tally = load::drive(&target, CONCURRENCY, ctx.duration);
            let paused = pause.map(|p| p.join().unwrap_or(Err("the pause panicked".to_string())));
            (tally, paused)
        });
        let ended = Instant::now();
        let seconds = started.elapsed().as_secs_f64();
        match paused {
            None => {}
            Some(Ok((paused, resumed))) => {
                let r = netem::recovery(started, ended, paused, resumed, &tally.done);
                println!(
                    "bench: pause {name} run={} rate_before={:.0} recovery_ms={} gap_ms={}",
                    ctx.run,
                    r.rate_before,
                    r.recovery
                        .map(|d| d.as_millis().to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    r.gap.as_millis()
                );
            }
            Some(Err(e)) => {
                fail(name, Reason::Netem, &e);
                return None;
            }
        }
        thread::sleep(SETTLE);
        let after = server.cpu_ticks();
        let rss = server.rss_kib();
        let (before, after) = match (before, after) {
            (Ok(b), Ok(a)) => (b, a),
            (Err(e), _) | (_, Err(e)) => {
                match server.exited() {
                    Some(status) => fail(
                        name,
                        Reason::ServerExited,
                        &format!("{status}: {}", server.last_lines()),
                    ),
                    None => fail(name, Reason::Proc, &e),
                }
                return None;
            }
        };
        let cpu = measure::cpu_us_per_req(after.saturating_sub(before), tally.requests);
        (tally, seconds, cpu, rss)
    };
    let (syscalls, strace_errors) = match strace::pass(server, &target, CONCURRENCY, ctx.duration) {
        Ok(t) => {
            if t.tally.errors > 0 {
                fail(
                    name,
                    Reason::Errors,
                    &format!(
                        "{} errors in the strace pass of run {}: {}",
                        t.tally.errors,
                        ctx.run,
                        t.tally.first_error.as_deref().unwrap_or("")
                    ),
                );
            }
            if ctx.syscalls_only {
                // The strace pass is the only load: its requests count.
                tally.requests = t.tally.requests;
            }
            (
                measure::per_request(t.calls, t.tally.requests),
                t.tally.errors,
            )
        }
        Err(e) => {
            fail(name, Reason::Strace, &e);
            (None, 0)
        }
    };
    // An idle connection the server dropped would make the window
    // measure fewer: each one counts as an error.
    let dropped = load::closed_idle(&idle) as u64;
    drop(idle);
    if dropped > 0 {
        fail(
            name,
            Reason::Errors,
            &format!("{dropped} idle connections closed in run {}", ctx.run),
        );
    }
    if tally.errors > 0 {
        fail(
            name,
            Reason::Errors,
            &format!(
                "{} errors in run {}: {}",
                tally.errors,
                ctx.run,
                tally.first_error.as_deref().unwrap_or("")
            ),
        );
    } else if tally.requests == 0 {
        fail(name, Reason::NoRequests, &format!("run {}", ctx.run));
    }
    let row = Row {
        date: ctx.date.clone(),
        commit: ctx.commit.clone(),
        runner: ctx.runner.clone(),
        scenario: name,
        requests: tally.requests,
        errors: tally.errors + strace_errors + dropped,
        seconds,
        cpu_us_per_req: cpu,
        syscalls_per_req: syscalls,
        rss_kib: rss,
    };
    println!(
        "bench: {name} requests={} errors={} req/s={:.0} cpu_us_per_req={} syscalls_per_req={} rss_kib={}",
        row.requests,
        row.errors,
        row.requests as f64 / seconds,
        row.cpu_us_per_req
            .map(|v| format!("{v:.2}"))
            .unwrap_or_default(),
        row.syscalls_per_req
            .map(|v| format!("{v:.2}"))
            .unwrap_or_default(),
        row.rss_kib.map(|v| v.to_string()).unwrap_or_default(),
    );
    Some(row)
}

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bench: {e}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    // A value that is not UTF-8 is refused like any other bad label.
    let runner_env = std::env::var_os("BENCH_RUNNER");
    let runner_env = runner_env
        .as_ref()
        .map(|v| v.to_str().unwrap_or("\u{fffd}"));
    let runner = match cli::runner_label(runner_env) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bench: {e}");
            return ExitCode::from(2);
        }
    };
    // `--netem`: the server behind the veth pair; the backend on our end.
    let (net, place, backend_ip) = match &args.netem {
        None => (
            None,
            Place::Loopback,
            std::net::IpAddr::from([127, 0, 0, 1]),
        ),
        Some(spec) => {
            if !server::is_root() {
                fail("all", Reason::Startup, "--netem needs root");
                return ExitCode::FAILURE;
            }
            match netem::Net::up(spec) {
                Ok(net) => (Some(net), Place::Netns, netem::CLIENT_IP),
                Err(e) => {
                    fail("all", Reason::Netem, &e);
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let backend = match Backend::start(backend_ip) {
        Ok(b) => b,
        Err(e) => {
            fail("all", Reason::Startup, &format!("backend: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let mut ctx = Context {
        date: row::today(),
        commit: cli::commit_label(args.commit.as_deref()),
        runner,
        duration: args.duration,
        run: 0,
        syscalls_only: args.syscalls_only,
        net,
        pause: args.pause,
    };
    let idle = args.scenarios.iter().map(|s| s.idle()).max().unwrap_or(0);
    if let Err(e) = match idle {
        0 => Ok(()),
        n => fds::ensure(n as u64 + fds::MARGIN),
    } {
        fail("all", Reason::Startup, &e);
        return ExitCode::FAILURE;
    }
    let mut rows = Vec::new();
    let mut failed = false;
    'runs: for run in 1..=args.runs {
        ctx.run = run;
        println!("bench: run {run} of {}", args.runs);
        // `proxy-file` runs on a server of its own: its `/file` route
        // (FR-PROXY-32) grows every connection's head buffer, so every
        // other scenario keeps the config it ran before stage 8. One
        // server at a time (`--netem` gives each the same address).
        for routes in [Routes::Plain, Routes::WithFiles] {
            let group: Vec<Scenario> = args
                .scenarios
                .iter()
                .copied()
                .filter(|s| s.routes() == routes)
                .collect();
            if group.is_empty() {
                continue;
            }
            let tag = match routes {
                Routes::Plain => run.to_string(),
                Routes::WithFiles => format!("{run}-files"),
            };
            let mut server = match Server::start(&args.binary, backend.addr, &tag, place, routes) {
                Ok(s) => s,
                Err(e) => {
                    fail("all", Reason::Startup, &e);
                    failed = true;
                    break 'runs;
                }
            };
            for scenario in group {
                match measure(&ctx, &mut server, scenario) {
                    Some(row) => {
                        failed |=
                            row.errors > 0 || row.requests == 0 || row.syscalls_per_req.is_none();
                        rows.push(row);
                    }
                    None => failed = true,
                }
                if let Err(e) = write_csv(&args.out, &rows) {
                    eprintln!("bench: {}: {e}", args.out.display());
                    return ExitCode::FAILURE;
                }
                if let Some(status) = server.exited() {
                    fail(
                        scenario.name(),
                        Reason::ServerExited,
                        &format!("{status}: {}", server.last_lines()),
                    );
                    failed = true;
                    break;
                }
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
