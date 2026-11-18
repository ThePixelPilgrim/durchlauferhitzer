// THROWAWAY spike TEST TOOL: TLS 1.3 load client (rustls + ring).
// loadclient ADDR CA.der THREADS SECONDS [--path /] [--same-write]
// Prints ok/fail counts, handshake rate, latency percentiles, and per-version
// first/last times (for the update-without-restart test).
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, ServerName};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let addr = a[1].clone();
    let ca = std::fs::read(&a[2]).unwrap();
    let threads: usize = a[3].parse().unwrap();
    let secs: f64 = a[4].parse().unwrap();
    let path = a.iter().position(|x| x == "--path").map(|i| a[i + 1].clone()).unwrap_or("/".into());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca)).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg.resumption = rustls::client::Resumption::disabled();
    let cfg = Arc::new(cfg);
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f64(secs);
    #[derive(Default)]
    struct St { ok: u64, fail: u64, lat: Vec<f64>, vers: BTreeMap<String, (u64, f64, f64)>, errs: Vec<(f64, String)> }
    let st = Arc::new(Mutex::new(St::default()));
    let mut hs = vec![];
    for _ in 0..threads {
        let cfg = cfg.clone();
        let st = st.clone();
        let addr = addr.clone();
        let path = path.clone();
        hs.push(std::thread::spawn(move || {
            while Instant::now() < deadline {
                let t0 = Instant::now();
                let r = (|| -> Result<String, String> {
                    let mut sock = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
                    sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
                    sock.set_nodelay(true).ok();
                    let mut c = rustls::ClientConnection::new(cfg.clone(), ServerName::try_from("localhost").unwrap()).map_err(|e| e.to_string())?;
                    // Queued before the handshake: rustls sends it right after the client Finished.
                    c.writer().write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
                    let mut tls = rustls::Stream::new(&mut c, &mut sock);
                    let mut body = Vec::new();
                    match tls.read_to_end(&mut body) {
                        Ok(_) => {}
                        Err(e) => return Err(format!("{e}")),
                    }
                    let s = String::from_utf8_lossy(&body);
                    if !s.starts_with("HTTP/1.1 200") { return Err("bad response".into()) }
                    Ok(s.lines().last().unwrap_or("").trim().trim_start_matches("hello from ").to_string())
                })();
                let now = start.elapsed().as_secs_f64();
                let mut g = st.lock().unwrap();
                match r {
                    Ok(v) => {
                        g.ok += 1;
                        g.lat.push(t0.elapsed().as_secs_f64() * 1000.0);
                        let e = g.vers.entry(v).or_insert((0, now, now));
                        e.0 += 1; e.2 = now;
                    }
                    Err(e) => { g.fail += 1; g.errs.push((now, e)); }
                }
            }
        }));
    }
    for h in hs { h.join().unwrap(); }
    let el = start.elapsed().as_secs_f64();
    let mut g = st.lock().unwrap();
    g.lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| if g.lat.is_empty() { 0.0 } else { g.lat[((g.lat.len() - 1) as f64 * q) as usize] };
    println!("ok={} fail={} elapsed={:.2}s rate={:.0}/s lat_ms p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        g.ok, g.fail, el, g.ok as f64 / el, p(0.5), p(0.9), p(0.99), p(1.0));
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = stat.rsplit(") ").next().unwrap_or("").split(' ').collect();
    let ticks: u64 = f.get(11).and_then(|x| x.parse().ok()).unwrap_or(0) + f.get(12).and_then(|x| x.parse().ok()).unwrap_or(0);
    println!("client_cpu_ticks={ticks} client_cpu_us_per_conn={}", ticks * 10000 / g.ok.max(1));
    for (v, (n, f, l)) in &g.vers { println!("version {v}: n={n} first={f:.3}s last={l:.3}s"); }
    for (t, e) in g.errs.iter().take(20) { println!("error at {t:.3}s: {e}"); }
}
