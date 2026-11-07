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
mod load;
mod measure;
mod row;
mod scenario;
mod server;
mod strace;

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use backend::Backend;
use load::Target;
use row::Row;
use scenario::Scenario;
use server::Server;

/// Client threads per scenario.
const CONCURRENCY: usize = 8;
/// The longest warm-up.
const WARM_UP: Duration = Duration::from_secs(1);
/// Time for the server to finish the closes after the load, inside the
/// measured window.
const SETTLE: Duration = Duration::from_millis(100);

/// Why a scenario failed (the fingerprint's reason).
#[derive(Debug, Clone, Copy)]
enum Reason {
    Startup,
    Errors,
    NoRequests,
    Proc,
    Strace,
    ServerExited,
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
        }
    }
}

fn fail(scenario: &str, reason: Reason, detail: &str) {
    println!(
        "bench failed: scenario={scenario} reason={} ({detail})",
        reason.name()
    );
}

/// The measured build: `BENCH_COMMIT`, else the checkout's short SHA.
fn commit() -> String {
    if let Ok(c) = std::env::var("BENCH_COMMIT") {
        return c;
    }
    Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
        | Scenario::StaticSmall1kIdle => None,
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
        let tally = load::drive(&target, CONCURRENCY, ctx.duration);
        let seconds = started.elapsed().as_secs_f64();
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
    let backend = match Backend::start() {
        Ok(b) => b,
        Err(e) => {
            fail("all", Reason::Startup, &format!("backend: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let mut ctx = Context {
        date: row::today(),
        commit: commit(),
        runner: std::env::var("BENCH_RUNNER").unwrap_or_else(|_| "local".to_string()),
        duration: args.duration,
        run: 0,
        syscalls_only: args.syscalls_only,
    };
    let mut rows = Vec::new();
    let mut failed = false;
    for run in 1..=args.runs {
        ctx.run = run;
        println!("bench: run {run} of {}", args.runs);
        let mut server = match Server::start(&args.binary, backend.addr, &run.to_string()) {
            Ok(s) => s,
            Err(e) => {
                fail("all", Reason::Startup, &e);
                failed = true;
                break;
            }
        };
        for &scenario in &args.scenarios {
            match measure(&ctx, &mut server, scenario) {
                Some(row) => {
                    failed |= row.errors > 0 || row.requests == 0 || row.syscalls_per_req.is_none();
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
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
