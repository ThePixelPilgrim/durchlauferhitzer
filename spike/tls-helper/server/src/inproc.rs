// THROWAWAY spike: baseline - the handshake in the long-lived server process.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, OnceLock};

use p256::ecdsa::signature::Signer as _;
use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert, ServerConfig, ServerConnection};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{ConnectionTrafficSecrets, SignatureAlgorithm, SignatureScheme};

use crate::record::Conn;
use crate::{sys, Opts};

static BASE: OnceLock<ServerConfig> = OnceLock::new();
static KEY: OnceLock<p256::ecdsa::SigningKey> = OnceLock::new();
static VIA_KEYD: OnceLock<bool> = OnceLock::new();

#[derive(Debug)]
struct Key;
#[derive(Debug)]
struct Sig;
impl SigningKey for Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256).then(|| Box::new(Sig) as Box<dyn Signer>)
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}
impl Signer for Sig {
    fn sign(&self, m: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        if *VIA_KEYD.get().unwrap_or(&false) {
            let fd = sys::connect_abstract(sys::KEYD_ABSTRACT).map_err(|e| rustls::Error::General(e.to_string()))?;
            sys::send_fds(fd, m, &[]).map_err(|e| rustls::Error::General(e.to_string()))?;
            let mut b = [0u8; 256];
            let r = sys::recv_fds(fd, &mut b).map_err(|e| rustls::Error::General(e.to_string()));
            unsafe { libc::close(fd) };
            let r = r?;
            if r.len == 0 { return Err(rustls::Error::General("keyd refused".into())) }
            return Ok(b[..r.len].to_vec());
        }
        let k = KEY.get().ok_or_else(|| rustls::Error::General("no key".into()))?;
        let s: p256::ecdsa::Signature = k.sign(m);
        Ok(s.to_der().as_bytes().to_vec())
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

#[derive(Default, Debug)]
struct Cap(Mutex<(Vec<u8>, Vec<u8>)>);
impl rustls::KeyLog for Cap {
    fn log(&self, label: &str, _: &[u8], secret: &[u8]) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match label {
            "CLIENT_TRAFFIC_SECRET_0" => s.0 = secret.to_vec(),
            "SERVER_TRAFFIC_SECRET_0" => s.1 = secret.to_vec(),
            _ => {}
        }
    }
    fn will_log(&self, l: &str) -> bool {
        l.ends_with("TRAFFIC_SECRET_0")
    }
}

pub fn prepare(o: &Opts) {
    let der = std::fs::read(&o.chain).expect("chain");
    if !o.key.is_empty() {
        let k = std::fs::read(&o.key).expect("key");
        let _ = KEY.set(p256::ecdsa::SigningKey::from_slice(&k[7..39]).expect("key"));
    }
    let _ = VIA_KEYD.set(o.inproc_keyd);
    let mut c = ServerConfig::builder_with_provider(spike_provider::provider_arc())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("versions")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(OneCert(Arc::new(CertifiedKey::new(vec![CertificateDer::from(der)], Arc::new(Key))))));
    c.send_tls13_tickets = 0;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    c.enable_secret_extraction = true;
    let _ = BASE.set(c);
}

struct W(RawFd);
impl Write for W {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let r = unsafe { libc::send(self.0, b.as_ptr().cast(), b.len(), libc::MSG_NOSIGNAL) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

pub fn conn(fd: RawFd, _o: &Opts) -> io::Result<(Conn, String)> {
    let r = conn_inner(fd);
    if r.is_err() { unsafe { libc::close(fd) }; }
    r
}

fn conn_inner(fd: RawFd) -> io::Result<(Conn, String)> {
    let mut cfg = BASE.get().ok_or_else(|| io::Error::other("no config"))?.clone();
    let cap = Arc::new(Cap::default());
    cfg.key_log = cap.clone();
    let mut c = ServerConnection::new(Arc::new(cfg)).map_err(io::Error::other)?;
    let mut w = W(fd);
    let mut rec = vec![0u8; 0];
    while c.is_handshaking() {
        while c.wants_write() { c.write_tls(&mut w)?; }
        if !c.is_handshaking() { break }
        rec.resize(5, 0);
        sys::read_full(fd, &mut rec[..5])?;
        let len = u16::from_be_bytes([rec[3], rec[4]]) as usize;
        rec.resize(5 + len, 0);
        sys::read_full(fd, &mut rec[5..])?;
        let mut s = &rec[..];
        while !s.is_empty() { c.read_tls(&mut s)?; }
        if let Err(e) = c.process_new_packets() {
            while c.wants_write() { if c.write_tls(&mut w).is_err() { break } }
            return Err(io::Error::other(e));
        }
    }
    while c.wants_write() { c.write_tls(&mut w)?; }
    let suite = c.negotiated_cipher_suite().map(|s| u16::from(s.suite())).unwrap_or(0);
    let ex = c.dangerous_extract_secrets().map_err(io::Error::other)?;
    let mut m = [0u8; sys::HANDOVER_LEN];
    m[..6].copy_from_slice(b"inproc");
    m[16..18].copy_from_slice(&suite.to_le_bytes());
    m[18..26].copy_from_slice(&ex.tx.0.to_le_bytes());
    m[26..34].copy_from_slice(&ex.rx.0.to_le_bytes());
    {
        let s = cap.0.lock().unwrap_or_else(|e| e.into_inner());
        m[34..66].copy_from_slice(&s.0);
        m[66..98].copy_from_slice(&s.1);
    }
    for (at, s) in [(98, &ex.tx.1), (142, &ex.rx.1)] {
        if let ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } | ConnectionTrafficSecrets::Aes128Gcm { key, iv } = s {
            m[at..at + key.as_ref().len()].copy_from_slice(key.as_ref());
            m[at + 32..at + 44].copy_from_slice(iv.as_ref());
        }
    }
    crate::handover_to_conn(fd, &m)
}
