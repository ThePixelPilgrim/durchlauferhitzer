//! THROWAWAY spike: a minimal rustls `CryptoProvider` over RustCrypto
//! primitives. TLS 1.3 only: TLS13_CHACHA20_POLY1305_SHA256 and
//! TLS13_AES_128_GCM_SHA256, X25519, no signature verification (server
//! without client auth), no private-key loading (signing is delegated).

use std::sync::Arc;

#[cfg(feature = "aes")]
use aes_gcm::Aes128Gcm;
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::consts::{U12, U16};
use hmac::Mac;
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, MessageDecrypter, MessageEncrypter,
    Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls13AeadAlgorithm,
    UnsupportedOperationError, make_tls13_aad,
};
use rustls::crypto::{
    self, ActiveKeyExchange, CryptoProvider, GetRandomFailed, KeyProvider, SecureRandom,
    SharedSecret, SupportedKxGroup, WebPkiSupportedAlgorithms, hash, hmac as rhmac,
};
use rustls::pki_types::PrivateKeyDer;
use rustls::{
    CipherSuite, CipherSuiteCommon, ConnectionTrafficSecrets, ContentType, Error, NamedGroup,
    ProtocolVersion, SupportedCipherSuite, Tls13CipherSuite,
};
use sha2::Digest;

// ---------------------------------------------------------------- provider

pub fn provider() -> CryptoProvider {
    CryptoProvider {
        #[cfg(feature = "aes")]
        cipher_suites: vec![CHACHA20_POLY1305, AES_128_GCM],
        #[cfg(not(feature = "aes"))]
        cipher_suites: vec![CHACHA20_POLY1305],
        kx_groups: vec![&X25519],
        signature_verification_algorithms: WebPkiSupportedAlgorithms {
            all: &[],
            mapping: &[],
        },
        secure_random: &Rng,
        key_provider: &NoKeys,
    }
}

pub fn provider_arc() -> Arc<CryptoProvider> {
    Arc::new(provider())
}

// ---------------------------------------------------------------- random

#[derive(Debug)]
struct Rng;

impl SecureRandom for Rng {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        fill_random(buf).map_err(|_| GetRandomFailed)
    }
}

/// getrandom(2) directly through libc; no `getrandom` crate.
pub fn fill_random(buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        // SAFETY: valid pointer/length into `buf`.
        let n = unsafe {
            libc::getrandom(
                buf[done..].as_mut_ptr().cast(),
                buf.len() - done,
                0,
            )
        };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(());
        }
        done += n as usize;
    }
    Ok(())
}

#[derive(Debug)]
struct NoKeys;

impl KeyProvider for NoKeys {
    fn load_private_key(
        &self,
        _key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn rustls::sign::SigningKey>, Error> {
        Err(Error::General("this provider never holds private keys".into()))
    }
}

// ---------------------------------------------------------------- X25519

#[derive(Debug)]
struct X25519Group;
static X25519: X25519Group = X25519Group;

struct X25519Active {
    secret: [u8; 32],
    public: [u8; 32],
}

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let mut secret = [0u8; 32];
        fill_random(&mut secret).map_err(|_| Error::FailedToGetRandomBytes)?;
        let public = x25519_dalek::x25519(secret, x25519_dalek::X25519_BASEPOINT_BYTES);
        Ok(Box::new(X25519Active { secret, public }))
    }
    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

impl ActiveKeyExchange for X25519Active {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        let peer: [u8; 32] = peer
            .try_into()
            .map_err(|_| Error::from(rustls::PeerMisbehaved::InvalidKeyShare))?;
        let shared = x25519_dalek::x25519(self.secret, peer);
        if shared == [0u8; 32] {
            return Err(rustls::PeerMisbehaved::InvalidKeyShare.into());
        }
        Ok(SharedSecret::from(&shared[..]))
    }
    fn pub_key(&self) -> &[u8] {
        &self.public
    }
    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

impl Drop for X25519Active {
    fn drop(&mut self) {
        self.secret = [0; 32];
    }
}

// ---------------------------------------------------------------- SHA-256 / HMAC

struct Sha256Hash;
static SHA256: Sha256Hash = Sha256Hash;

struct Sha256Ctx(sha2::Sha256);

impl hash::Hash for Sha256Hash {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Ctx(sha2::Sha256::new()))
    }
    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&sha2::Sha256::digest(data)[..])
    }
    fn output_len(&self) -> usize {
        32
    }
    fn algorithm(&self) -> hash::HashAlgorithm {
        hash::HashAlgorithm::SHA256
    }
}

impl hash::Context for Sha256Ctx {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize()[..])
    }
    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(Sha256Ctx(self.0.clone()))
    }
    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize()[..])
    }
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

struct HmacSha256;
static HMAC_SHA256: HmacSha256 = HmacSha256;
struct HmacKey(hmac::Hmac<sha2::Sha256>);

impl rhmac::Hmac for HmacSha256 {
    fn with_key(&self, key: &[u8]) -> Box<dyn rhmac::Key> {
        // HMAC accepts keys of any length.
        Box::new(HmacKey(
            <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!()),
        ))
    }
    fn hash_output_len(&self) -> usize {
        32
    }
}

impl rhmac::Key for HmacKey {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> rhmac::Tag {
        let mut m = self.0.clone();
        m.update(first);
        for x in middle {
            m.update(x);
        }
        m.update(last);
        rhmac::Tag::new(&m.finalize().into_bytes()[..])
    }
    fn tag_len(&self) -> usize {
        32
    }
}

static HKDF_SHA256: crypto::tls13::HkdfUsingHmac<'static> =
    crypto::tls13::HkdfUsingHmac(&HMAC_SHA256);

// ---------------------------------------------------------------- AEADs

pub static CHACHA20_POLY1305: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            hash_provider: &SHA256,
            confidentiality_limit: u64::MAX,
        },
        hkdf_provider: &HKDF_SHA256,
        aead_alg: &ChachaAlg,
        quic: None,
    });

#[cfg(feature = "aes")]
pub static AES_128_GCM: SupportedCipherSuite = SupportedCipherSuite::Tls13(&Tls13CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
        hash_provider: &SHA256,
        confidentiality_limit: 1 << 24,
    },
    hkdf_provider: &HKDF_SHA256,
    aead_alg: &AesAlg,
    quic: None,
});

struct ChachaAlg;
#[cfg(feature = "aes")]
struct AesAlg;

impl Tls13AeadAlgorithm for ChachaAlg {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Cipher(
            ChaCha20Poly1305::new_from_slice(key.as_ref()).unwrap_or_else(|_| unreachable!()),
            iv,
        ))
    }
    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Cipher(
            ChaCha20Poly1305::new_from_slice(key.as_ref()).unwrap_or_else(|_| unreachable!()),
            iv,
        ))
    }
    fn key_len(&self) -> usize {
        32
    }
    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv })
    }
}

#[cfg(feature = "aes")]
impl Tls13AeadAlgorithm for AesAlg {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Cipher(
            Aes128Gcm::new_from_slice(key.as_ref()).unwrap_or_else(|_| unreachable!()),
            iv,
        ))
    }
    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Cipher(
            Aes128Gcm::new_from_slice(key.as_ref()).unwrap_or_else(|_| unreachable!()),
            iv,
        ))
    }
    fn key_len(&self) -> usize {
        16
    }
    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Aes128Gcm { key, iv })
    }
}

struct Tls13Cipher<A>(A, Iv);

impl<A: AeadInOut<NonceSize = U12, TagSize = U16> + Send + Sync> MessageEncrypter for Tls13Cipher<A> {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let nonce = Nonce::new(&self.1, seq);
        let aad = make_tls13_aad(total_len);
        let tag = self
            .0
            .encrypt_inout_detached(&nonce.0.into(), &aad, payload.as_mut().into())
            .map_err(|_| Error::EncryptError)?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            payload,
        ))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + 16
    }
}

impl<A: AeadInOut<NonceSize = U12, TagSize = U16> + Send + Sync> MessageDecrypter for Tls13Cipher<A> {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut msg.payload;
        let len = payload.len();
        if len < 16 {
            return Err(Error::DecryptError);
        }
        let nonce = Nonce::new(&self.1, seq);
        let aad = make_tls13_aad(len);
        let (body, tag) = payload.split_at_mut(len - 16);
        let tag: [u8; 16] = (&*tag).try_into().map_err(|_| Error::DecryptError)?;
        self.0
            .decrypt_inout_detached(&nonce.0.into(), &aad, body.into(), &tag.into())
            .map_err(|_| Error::DecryptError)?;
        payload.truncate(len - 16);
        msg.into_tls13_unpadded_message()
    }
}
