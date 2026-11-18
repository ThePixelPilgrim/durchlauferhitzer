// THROWAWAY spike: minimal TLS 1.3 record layer (server side) from exported secrets.

use std::io;
use std::os::fd::RawFd;

use aes_gcm::Aes128Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use hmac::Mac;

use crate::sys;

pub const CT_ALERT: u8 = 21;
pub const CT_HANDSHAKE: u8 = 22;
pub const CT_APPDATA: u8 = 23;
const MAX_PLAIN: usize = 16384;

enum Aead {
    Chacha(ChaCha20Poly1305),
    Aes(Aes128Gcm),
}

pub struct Dir {
    aead: Aead,
    iv: [u8; 12],
    pub seq: u64,
    secret: [u8; 32],
    suite: u16,
}

fn hmac256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// HKDF-Expand-Label(secret, label, "", len) for len <= 32 (one HMAC block).
pub fn expand_label(secret: &[u8; 32], label: &str, len: usize) -> [u8; 32] {
    let full = format!("tls13 {label}");
    let mut info = Vec::with_capacity(4 + full.len());
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push(full.len() as u8);
    info.extend_from_slice(full.as_bytes());
    info.push(0);
    let mut out = hmac256(secret, &[&info, &[1]]);
    for b in out.iter_mut().skip(len) {
        *b = 0;
    }
    out
}

impl Dir {
    pub fn from_secret(suite: u16, secret: [u8; 32], seq: u64) -> io::Result<Dir> {
        let key_len = match suite {
            0x1303 => 32,
            0x1301 => 16,
            _ => return Err(io::Error::other("unsupported suite")),
        };
        let key = expand_label(&secret, "key", key_len);
        let ivb = expand_label(&secret, "iv", 12);
        let mut iv = [0u8; 12];
        iv.copy_from_slice(&ivb[..12]);
        let aead = match suite {
            0x1303 => Aead::Chacha(ChaCha20Poly1305::new_from_slice(&key[..32]).map_err(io::Error::other)?),
            _ => Aead::Aes(Aes128Gcm::new_from_slice(&key[..16]).map_err(io::Error::other)?),
        };
        Ok(Dir { aead, iv, seq, secret, suite })
    }

    pub fn key_iv(&self) -> ([u8; 32], [u8; 12]) {
        let kl = if self.suite == 0x1303 { 32 } else { 16 };
        (expand_label(&self.secret, "key", kl), self.iv)
    }

    /// KeyUpdate: application_traffic_secret_N+1 = HKDF-Expand-Label(secret_N, "traffic upd", "", 32)
    pub fn update(&mut self) -> io::Result<()> {
        let next = expand_label(&self.secret, "traffic upd", 32);
        *self = Dir::from_secret(self.suite, next, 0)?;
        Ok(())
    }

    fn nonce(&self) -> [u8; 12] {
        let mut n = self.iv;
        for (i, b) in self.seq.to_be_bytes().iter().enumerate() {
            n[4 + i] ^= b;
        }
        n
    }
}

pub struct Conn {
    pub fd: RawFd,
    pub rx: Dir,
    pub tx: Dir,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    pub key_updates_rx: u32,
}

pub enum Rec {
    Data(Vec<u8>),
    Closed,
}

impl Conn {
    pub fn new(fd: RawFd, rx: Dir, tx: Dir) -> Conn {
        Conn { fd, rx, tx, rbuf: Vec::with_capacity(MAX_PLAIN + 256 + 5), wbuf: Vec::with_capacity(MAX_PLAIN + 64), key_updates_rx: 0 }
    }

    /// Encrypt and send one record (plaintext <= 16 KiB).
    pub fn send(&mut self, ct: u8, data: &[u8]) -> io::Result<()> {
        let total = data.len() + 1 + 16;
        let hdr = [CT_APPDATA, 3, 3, (total >> 8) as u8, total as u8];
        self.wbuf.clear();
        self.wbuf.extend_from_slice(&hdr);
        self.wbuf.extend_from_slice(data);
        self.wbuf.push(ct);
        let nonce = self.tx.nonce();
        let body = &mut self.wbuf[5..];
        let tag = match &self.tx.aead {
            Aead::Chacha(a) => a.encrypt_inout_detached(&nonce.into(), &hdr, body.into()),
            Aead::Aes(a) => a.encrypt_inout_detached(&nonce.into(), &hdr, body.into()),
        }
        .map_err(|_| io::Error::other("encrypt"))?;
        self.wbuf.extend_from_slice(&tag);
        self.tx.seq += 1;
        sys::write_full(self.fd, &self.wbuf)
    }

    pub fn send_data(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = data.len().min(MAX_PLAIN);
            self.send(CT_APPDATA, &data[..n])?;
            data = &data[n..];
        }
        Ok(())
    }

    pub fn close_notify(&mut self) -> io::Result<()> {
        self.send(CT_ALERT, &[1, 0])
    }

    /// Read one record; handles KeyUpdate and alerts itself. Returns
    /// application data, or Closed on close_notify/EOF.
    pub fn recv(&mut self) -> io::Result<Rec> {
        loop {
            self.rbuf.resize(5, 0);
            match sys::read_full(self.fd, &mut self.rbuf[..5]) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Rec::Closed),
                Err(e) => return Err(e),
            }
            let hdr: [u8; 5] = [self.rbuf[0], self.rbuf[1], self.rbuf[2], self.rbuf[3], self.rbuf[4]];
            let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
            if len > MAX_PLAIN + 256 || len < 17 {
                return Err(io::Error::other("bad record length"));
            }
            if hdr[0] == 20 {
                // change_cipher_spec compatibility record: skip
                self.rbuf.resize(5 + len, 0);
                sys::read_full(self.fd, &mut self.rbuf[5..])?;
                continue;
            }
            if hdr[0] != CT_APPDATA {
                return Err(io::Error::other(format!("unexpected outer type {}", hdr[0])));
            }
            self.rbuf.resize(5 + len, 0);
            sys::read_full(self.fd, &mut self.rbuf[5..])?;
            let nonce = self.rx.nonce();
            let (body, tag) = self.rbuf[5..].split_at_mut(len - 16);
            let tag: [u8; 16] = (&*tag).try_into().map_err(|_| io::Error::other("tag"))?;
            match &self.rx.aead {
                Aead::Chacha(a) => a.decrypt_inout_detached(&nonce.into(), &hdr, body.into(), &tag.into()),
                Aead::Aes(a) => a.decrypt_inout_detached(&nonce.into(), &hdr, body.into(), &tag.into()),
            }
            .map_err(|_| io::Error::other(format!("bad_record_mac at rx seq {}", self.rx.seq)))?;
            self.rx.seq += 1;
            // strip zero padding; last non-zero byte is the inner content type
            let mut end = len - 16;
            while end > 0 && body[end - 1] == 0 {
                end -= 1;
            }
            if end == 0 {
                return Err(io::Error::other("no inner content type"));
            }
            let ct = body[end - 1];
            let plain = body[..end - 1].to_vec();
            match ct {
                CT_APPDATA => return Ok(Rec::Data(plain)),
                CT_ALERT => {
                    if plain.len() == 2 && plain[1] == 0 {
                        return Ok(Rec::Closed);
                    }
                    return Err(io::Error::other(format!("peer alert {plain:?}")));
                }
                CT_HANDSHAKE => {
                    // Only KeyUpdate (type 24, length 1) is acceptable post-handshake here.
                    if plain.len() == 5 && plain[0] == 24 && plain[1..4] == [0, 0, 1] {
                        let requested = plain[4] == 1;
                        self.rx.update()?;
                        self.key_updates_rx += 1;
                        eprintln!("server: client KeyUpdate (update_requested={requested}), rx keys rotated");
                        if requested {
                            self.send(CT_HANDSHAKE, &[24, 0, 0, 1, 0])?;
                            self.tx.update()?;
                            eprintln!("server: sent own KeyUpdate, tx keys rotated");
                        }
                        continue;
                    }
                    let _ = self.send(CT_ALERT, &[2, 10]); // unexpected_message
                    return Err(io::Error::other(format!("unexpected post-handshake message type {}", plain.first().copied().unwrap_or(0))));
                }
                _ => return Err(io::Error::other("bad inner type")),
            }
        }
    }
}
