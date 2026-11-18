// Comparison only: does rustls-rustcrypto build and give a usable provider?
fn main() {
    let p = rustls_rustcrypto::provider();
    for s in &p.cipher_suites { println!("suite {:?}", s.suite()); }
    for k in &p.kx_groups { println!("kx {:?}", k.name()); }
    for (s, _) in p.signature_verification_algorithms.mapping { println!("sigverify {s:?}"); }
    let c = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(p)).with_protocol_versions(&[&rustls::version::TLS13]);
    println!("config builder: {}", c.is_ok());
}
