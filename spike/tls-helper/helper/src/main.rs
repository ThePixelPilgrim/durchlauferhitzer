// THROWAWAY spike: durchlauferhitzer-tls, the per-connection TLS handshake helper.
//
// Inherited fds:
//   3  inbox: SOCK_SEQPACKET from the spawner; one message carries
//      SCM_RIGHTS [client socket, control socket to the server]
//   4  key process connection (SOCK_SEQPACKET), connected by the spawner
//   5  certificate chain (memfd): repeated (u32 LE len, DER)
// argv[1] (optional): spawner's CLOCK_MONOTONIC ns just before execve.
// env SPIKE_SECCOMP=1 installs the seccomp allow-list after start-up.
// env SPIKE_ACCEPT_UIDS=0,2002 : uids allowed as inbox peer (default "0").

#[path = "../../common/sys.rs"]
mod sys;
mod seccomp;

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};

use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert, ServerConfig, ServerConnection};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{ConnectionTrafficSecrets, SignatureAlgorithm, SignatureScheme};

const VERSION: &str = match option_env!("SPIKE_HELPER_VERSION") {
    Some(v) => v,
    None => "helper-v1",
};

const INBOX: RawFd = 3;
const KEYD: RawFd = 4;
const CERTS: RawFd = 5;

fn die(msg: &str) -> ! {
    eprintln!("helper[{}] {VERSION}: {msg}", std::process::id());
    std::process::exit(1)
}

// ---------------------------------------------------------------- delegated signing

#[derive(Debug)]
struct RemoteKey;
#[derive(Debug)]
struct RemoteSigner;

impl SigningKey for RemoteKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(RemoteSigner) as Box<dyn Signer>)
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

impl Signer for RemoteSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        if std::env::var_os("SPIKE_DUMP_SIGN").is_some() {
            eprintln!("helper: asked to sign {} bytes: {}", message.len(), hex(message));
        }
        sys::send_fds(KEYD, message, &[])
            .map_err(|e| rustls::Error::General(format!("keyd send: {e}")))?;
        let mut buf = [0u8; 256];
        let r = sys::recv_fds(KEYD, &mut buf)
            .map_err(|e| rustls::Error::General(format!("keyd recv: {e}")))?;
        if r.len == 0 {
            return Err(rustls::Error::General("keyd refused (EOF)".into()));
        }
        Ok(buf[..r.len].to_vec())
    }
    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[derive(Debug)]
struct OneCert(Arc<CertifiedKey>);
impl ResolvesServerCert for OneCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

// ---------------------------------------------------------------- key log: traffic secrets

#[derive(Default)]
struct Secrets {
    client: Vec<u8>,
    server: Vec<u8>,
}
struct CaptureSecrets(Mutex<Secrets>);
impl fmt::Debug for CaptureSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaptureSecrets")
    }
}
impl rustls::KeyLog for CaptureSecrets {
    fn log(&self, label: &str, _client_random: &[u8], secret: &[u8]) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match label {
            "CLIENT_TRAFFIC_SECRET_0" => s.client = secret.to_vec(),
            "SERVER_TRAFFIC_SECRET_0" => s.server = secret.to_vec(),
            _ => {}
        }
    }
    fn will_log(&self, label: &str) -> bool {
        label.ends_with("TRAFFIC_SECRET_0")
    }
}

// ---------------------------------------------------------------- record-exact reader

/// Reads exactly one TLS record (5-byte header + body) from a blocking fd.
fn read_one_record(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.resize(5, 0);
    sys::read_full(fd, &mut buf[..5])?;
    let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if len > 16384 + 256 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "record too long"));
    }
    buf.resize(5 + len, 0);
    sys::read_full(fd, &mut buf[5..])?;
    Ok(())
}

struct FdWriter(RawFd);
impl Write for FdWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let r = unsafe { libc::send(self.0, b.as_ptr().cast(), b.len(), libc::MSG_NOSIGNAL) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn read_certs() -> Vec<CertificateDer<'static>> {
    let mut all = Vec::new();
    let mut off = 0i64;
    let mut buf = [0u8; 65536];
    loop {
        let r = unsafe { libc::pread(CERTS, buf.as_mut_ptr().cast(), buf.len(), off) };
        if r < 0 { die(&format!("cert pread: {}", io::Error::last_os_error())) }
        if r == 0 { break }
        all.extend_from_slice(&buf[..r as usize]);
        off += r as i64;
    }
    unsafe { libc::close(CERTS) };
    let mut out = Vec::new();
    let mut p = &all[..];
    while p.len() >= 4 {
        let n = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
        if p.len() < 4 + n { die("truncated cert chain") }
        out.push(CertificateDer::from(p[4..4 + n].to_vec()));
        p = &p[4 + n..];
    }
    if out.is_empty() { die("no certificate on fd 5") }
    out
}

fn main() {
    let t_main = sys::now_ns();
    let t_exec: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);

    let certs = read_certs();
    let t_certs = sys::now_ns();
    let secrets = Arc::new(CaptureSecrets(Mutex::new(Secrets::default())));
    let provider = spike_provider::provider_arc();
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap_or_else(|e| die(&format!("config: {e}")))
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(OneCert(Arc::new(CertifiedKey::new(certs, Arc::new(RemoteKey))))));
    config.send_tls13_tickets = 0;
    config.alpn_protocols = if VERSION.contains("v2") {
        vec![b"http/1.1".to_vec(), b"spike-v2".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    config.enable_secret_extraction = true;
    config.key_log = secrets.clone();
    config.max_early_data_size = 0;
    let config = Arc::new(config);
    let t_config = sys::now_ns();

    if std::env::var_os("SPIKE_SECCOMP").is_some() {
        seccomp::install().unwrap_or_else(|e| die(&format!("seccomp: {e}")));
    }

    // ---- wait for work
    let allowed: Vec<u32> = std::env::var("SPIKE_ACCEPT_UIDS")
        .unwrap_or_else(|_| "0".into())
        .split(',')
        .filter_map(|s| s.parse().ok())
        .collect();
    let pc = sys::peercred(INBOX).unwrap_or_else(|e| die(&format!("inbox peercred: {e}")));
    if !allowed.contains(&pc.uid) {
        die(&format!("REJECT inbox peer uid {} pid {} (allowed {allowed:?})", pc.uid, pc.pid));
    }
    let mut b = [0u8; 16];
    let rx = sys::recv_fds(INBOX, &mut b).unwrap_or_else(|e| die(&format!("inbox: {e}")));
    if rx.fds.len() != 2 { die(&format!("inbox: expected 2 fds, got {}", rx.fds.len())) }
    let (client, ctl) = (rx.fds[0], rx.fds[1]);
    unsafe { libc::close(INBOX) };
    let t_inbox = sys::now_ns();
    // A silent client must not hold the helper forever.
    let tv = libc::timeval { tv_sec: 10, tv_usec: 0 };
    unsafe {
        libc::setsockopt(client, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&tv as *const libc::timeval).cast(), 16);
    }

    // ---- handshake, one record at a time
    let mut conn = ServerConnection::new(config).unwrap_or_else(|e| die(&format!("conn: {e}")));
    let mut rec = Vec::with_capacity(16384 + 5);
    let mut w = FdWriter(client);
    let mut records_in = 0;
    while conn.is_handshaking() {
        while conn.wants_write() {
            if let Err(e) = conn.write_tls(&mut w) { die(&format!("write: {e}")) }
        }
        if !conn.is_handshaking() { break }
        if let Err(e) = read_one_record(client, &mut rec) {
            die(&format!("read after {records_in} records: {e}"));
        }
        records_in += 1;
        let mut slice = &rec[..];
        while !slice.is_empty() {
            if let Err(e) = conn.read_tls(&mut slice) { die(&format!("read_tls: {e}")) }
        }
        if let Err(e) = conn.process_new_packets() {
            // rustls has queued the alert; send it, then give up.
            while conn.wants_write() {
                if conn.write_tls(&mut w).is_err() { break }
            }
            die(&format!("handshake error: {e}"));
        }
    }
    while conn.wants_write() {
        if let Err(e) = conn.write_tls(&mut w) { die(&format!("write: {e}")) }
    }
    let t_hs = sys::now_ns();
    // Nothing may be left inside rustls: no plaintext, no buffered ciphertext.
    let mut probe = [0u8; 1];
    let leftover_plain = match conn.reader().read(&mut probe) {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
        Err(_) => 0,
    };
    if leftover_plain != 0 { die("plaintext stuck in helper") }
    let suite = conn.negotiated_cipher_suite().map(|s| u16::from(s.suite())).unwrap_or(0);
    let alpn = conn.alpn_protocol().map(|a| String::from_utf8_lossy(a).into_owned());
    let ex = conn.dangerous_extract_secrets().unwrap_or_else(|e| die(&format!("extract: {e}")));

    // ---- hand over
    let mut m = [0u8; sys::HANDOVER_LEN];
    let v = VERSION.as_bytes();
    m[..v.len().min(16)].copy_from_slice(&v[..v.len().min(16)]);
    m[16..18].copy_from_slice(&suite.to_le_bytes());
    m[18..26].copy_from_slice(&ex.tx.0.to_le_bytes());
    m[26..34].copy_from_slice(&ex.rx.0.to_le_bytes());
    {
        let s = secrets.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.client.len() != 32 || s.server.len() != 32 { die("key log missed the traffic secrets") }
        m[34..66].copy_from_slice(&s.client);
        m[66..98].copy_from_slice(&s.server);
    }
    let put = |m: &mut [u8], at: usize, s: &ConnectionTrafficSecrets| {
        let (k, iv) = match s {
            ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv }
            | ConnectionTrafficSecrets::Aes128Gcm { key, iv }
            | ConnectionTrafficSecrets::Aes256Gcm { key, iv } => (key.as_ref(), iv.as_ref()),
            _ => die("unknown secret kind"),
        };
        m[at..at + k.len()].copy_from_slice(k);
        m[at + 32..at + 44].copy_from_slice(iv);
    };
    put(&mut m, 98, &ex.tx.1);
    put(&mut m, 142, &ex.rx.1);
    for (i, t) in [t_main, t_config, t_inbox, t_hs].iter().enumerate() {
        m[186 + i * 8..194 + i * 8].copy_from_slice(&t.to_le_bytes());
    }
    m[218..226].copy_from_slice(&sys::maxrss_kib().to_le_bytes());
    if let Err(e) = sys::send_fds(ctl, &m, &[client]) { die(&format!("handover: {e}")) }
    if std::env::var_os("SPIKE_VERBOSE").is_some() {
        eprintln!(
            "helper[{}] {VERSION}: done suite={suite:#06x} alpn={alpn:?} records_in={records_in} \
             exec->main={}us certs={}us config={}us wait={}us handshake={}us maxrss={}KiB",
            std::process::id(),
            if t_exec > 0 { (t_main - t_exec) / 1000 } else { 0 },
            (t_certs - t_main) / 1000,
            (t_config - t_certs) / 1000,
            (t_inbox - t_config) / 1000,
            (t_hs - t_inbox) / 1000,
            sys::maxrss_kib()
        );
    }
    std::process::exit(0);
}
