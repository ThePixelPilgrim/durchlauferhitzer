// THROWAWAY spike: the long-lived key process. Holds the P-256 key, signs
// TLS 1.3 server CertificateVerify inputs for peers with the helper's uid.
// usage: keyd KEY.sec1.der   (run it as the key uid, e.g. via setpriv)
#[path = "../../common/sys.rs"]
mod sys;

use p256::ecdsa::{Signature, SigningKey, signature::Signer};

const CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify\0";

fn main() {
    let path = std::env::args().nth(1).expect("key path");
    let der = std::fs::read(&path).expect("read key");
    // SEC1 ECPrivateKey: 30 77 02 01 01 04 20 <32 bytes> ...
    assert!(der.len() > 39 && der[2..7] == [0x02, 0x01, 0x01, 0x04, 0x20], "not SEC1 P-256");
    let key = SigningKey::from_slice(&der[7..39]).expect("key");
    let allow: u32 = std::env::var("SPIKE_KEYD_ALLOW_UID").ok().and_then(|s| s.parse().ok()).unwrap_or(sys::HELPER_UID);
    let lfd = sys::listen_abstract(sys::KEYD_ABSTRACT).expect("listen");
    let dump = std::env::var_os("SPIKE_DUMP_SIGN").is_some();
    eprintln!("keyd: uid={} listening, signing only for uid {allow}", unsafe { libc::getuid() });
    let key: &'static SigningKey = Box::leak(Box::new(key));
    loop {
        let c = unsafe { libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 { continue }
        let pc = match sys::peercred(c) { Ok(p) => p, Err(_) => { unsafe { libc::close(c) }; continue } };
        if pc.uid != allow {
            eprintln!("keyd: REJECT peer uid {} pid {}", pc.uid, pc.pid);
            unsafe { libc::close(c) };
            continue;
        }
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let r = match sys::recv_fds(c, &mut buf) { Ok(r) if r.len > 0 => r, _ => break };
                let m = &buf[..r.len];
                // Accept only a CertificateVerify input with a SHA-256 transcript hash.
                let ok = m.len() == 64 + CONTEXT.len() + 32
                    && m[..64].iter().all(|&b| b == 0x20)
                    && &m[64..64 + CONTEXT.len()] == CONTEXT;
                if !ok {
                    eprintln!("keyd: REJECT non-CertificateVerify input of {} bytes from pid {}", m.len(), pc.pid);
                    break;
                }
                if dump { eprintln!("keyd: signing {} bytes for pid {}", m.len(), pc.pid); }
                let sig: Signature = key.sign(m);
                let d = sig.to_der();
                if sys::send_fds(c, d.as_bytes(), &[]).is_err() { break }
            }
            unsafe { libc::close(c) };
        });
    }
}
