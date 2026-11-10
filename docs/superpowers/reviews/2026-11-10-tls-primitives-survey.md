# TLS 1.3 handshake primitives: crate survey

Survey date 2026-11-10, for open decision 5 (TLS). Toolchain rustc/cargo 1.97.1. All numbers were measured in throwaway scratch projects outside the repo, one per set; their exact `Cargo.toml` are at the end of this document.
Versions, dates and MSRVs come from the crates.io API. Each set pins its crates with `=` and turns default features off.

**Method.**
- **Trees:** `cargo tree -e normal,build --locked --prefix none`, once with `--target all` (the flag `scripts/check-product-deps.sh` uses) and once with the four real product targets (`x86_64`/`aarch64` × `linux-gnu`/`linux-musl`).
- **Counted per crate in `~/.cargo/registry/src`:** build script (`package.build` or `build.rs`), `proc-macro = true`, and `unsafe` tokens. The `unsafe` count covers `.rs` files only. It excludes the top-level `tests/`, `benches/`, `examples/`, `fuzz/` and `proofs/` directories, strips `//`, `/* */` and string literals, and then counts the `\bunsafe\b` tokens. `#[cfg(test)]` modules are not excluded.
- **Assembly and C:** inline asm is counted as `asm!`/`global_asm!`/`naked_asm!` invocations. C and assembly are looked for as `.c`/`.h`/`.cc`/`.cpp`/`.S`/`.s`/`.asm` files, and as `cc::`/`cmake::`/`bindgen` in build scripts.
- **Compile check:** `RUSTFLAGS="-D warnings" cargo check --locked` passed for every set on 1.97.1. The set crates are empty `#![no_std]` libs, so dependency warnings are capped. This shows only that each set compiles.
- **Test vectors:** a separate test, `sets/rfc6979-check`, ran the RFC 6979 A.2.5 vector against both P-256 candidates.
- **Scripts:** throwaway scan scripts, not kept in the repo; the method above is what they did.

## Summary table

Crate counts exclude the set crate itself. "Linux" means the four product targets; "all" means `--target all`. The `unsafe` column excludes `libc` (731), which every set contains and the product already ships.

| Set (Cargo.toml below) | Crates linux / all | Proc macros (linux) | Build scripts (linux) | `unsafe`-bearing crates (excl. libc) / tokens | Inline asm compiled on x86_64 or aarch64 | C/C++ files (`--target all`) | Verified primitives (of 5) | Top-level crates with a safe-to-deploy audit importable today |
|---|---|---|---|---|---|---|---|---|
| A `rustcrypto` | 55 / 56 | 1 (curve25519-dalek-derive) | 5 (curve25519-dalek, libc, num-traits, proc-macro2, quote) | 24 / 546 | cmov (x86_64, aarch64), zeroize (barrier, all listed arches) | 0 | 0 (X25519 field arithmetic verified only with opt-in `fiat` backend) | 0 of 7 |
| A′ `rustcrypto-noaes` | 50 / 51 | 1 | 5 | 22 / 462 | cmov, zeroize | 0 | 0 | 0 of 6 |
| A+PEM `rustcrypto-pem` | 59 / 60 | 1 | 5 | 26 / 551 | cmov, zeroize | 0 | 0 | 0 of 7 |
| B `libcrux` | 23 / 75 | 1 (libcrux-macros) | 6 (libc, libcrux-aes, libcrux-intrinsics, libcrux-sha3, proc-macro2, quote) | 8 / 313 | libcrux-secrets (aarch64 only) | 2 crates: cc, crabgrind (cfg-gated) | 4 (SHA-256 family, ChaCha20-Poly1305, X25519, P-256 ECDSA core); AES-GCM "pre-verification"; no RFC 6979 | 0 of 8 |
| B′ `libcrux-noaes` | 21 / 73 | 1 | 5 | 8 / 313 | libcrux-secrets (aarch64) | 2 (cfg-gated) | 4 of 4 used | 0 of 6 |
| C `mixed` (libcrux; RustCrypto P-256 + AES-GCM) | 60 / 108 | 1 (libcrux-macros) | 6 | 24 / 604 | cmov, zeroize, libcrux-secrets | 2 (cfg-gated) | 3 (SHA-2 family, ChaCha20-Poly1305, X25519) | 0 of 7 |
| D `mixed-verified` (libcrux incl. P-256; RustCrypto AES-GCM) | 38 / 89 | 1 | 5 | 15 / 520 | cmov, libcrux-secrets | 2 (cfg-gated) | 4 + RFC 6979 ours | 0 of 7 |
| E `graviola` (excluded: asm) | 4 / 5 | 0 | 2 | 2 / 452 | graviola (149 `asm!`), getrandom | 0 | s2n-bignum asm verified (field/bignum) | 0 of 1 |
| F `orion` (no P-256, no AES-GCM) | 4 / 4 | 0 | 0 | 1 / 27 (+ cmov) | cmov | 0 | Poly1305 + X25519 arithmetic (fiat) | 0 of 1 |

Three facts the table compresses:
- **libcrux under `--target all`.** The libcrux crates declare dependencies under the custom cfgs `cfg(hax)` and `cfg(valgrind_ct_test)`. Those cfgs are never set in a normal build, but `--target all` lists their dependencies anyway: hax-lib and core-models, and crabgrind, which brings `cc` 1.11.1, `bindgen`, `clang-sys` and bundled valgrind `.c`/`.h` files. These deps add 52 crates, 13 build scripts and 6 proc macros to the `--target all` tree. `scripts/check-product-deps.sh` uses `--target all`, so it would flag all of them.
- **RustCrypto inline asm.** `cmov` uses an x86 `cmov` instruction and an aarch64 `csel` instruction through `asm!`. `ctutils` depends on `cmov`, and `digest`, `universal-hash`, `aes-gcm`, `crypto-bigint` and `sec1` depend on `ctutils`, so every RustCrypto set except bare `sha2` compiles inline asm on both product architectures. `zeroize`'s `optimization_barrier` is an empty `asm!` on x86_64/aarch64. The asm in `sha2` (riscv, loongarch) and `cpufeatures` (loongarch) is not compiled for the product targets.
- **libcrux inline asm.** `libcrux-secrets` uses asm only on aarch64; on x86_64 it uses `core::hint::black_box`.

## Family 1: RustCrypto (+ dalek)

| Crate | Version (date) | MSRV | Reverse deps | Maintainer | RUSTSEC history |
|---|---|---|---|---|---|
| sha2 | 0.11.1 (2026-10-12) | 1.88 | 24,728 | RustCrypto | RUSTSEC-2021-0100 (AVX2 miscompute, ≤0.9.7) |
| hmac | 0.13.0 (2026-03-29) | 1.85 | 4,689 | RustCrypto | none |
| hkdf | 0.13.0 (2026-03-30) | 1.85 | 1,296 | RustCrypto | none |
| chacha20poly1305 | 0.11.0 (2026-06-28) | 1.85 | 1,535 | RustCrypto | none (chacha20: RUSTSEC-2019-0029, <0.2.3) |
| aes-gcm | 0.11.1 (2026-08-21) | 1.85 (aes 0.9.4: 1.89) | 2,061 | RustCrypto | RUSTSEC-2023-0096 (plaintext exposed on tag failure, 0.10.0–0.10.2) |
| x25519-dalek | 3.0.0 (2026-07-06) | 1.85 | 952 | dalek-cryptography | none |
| curve25519-dalek | 5.0.0 (2026-07-06) | 1.85 | 676 | dalek-cryptography | RUSTSEC-2024-0344 (timing in `Scalar52::sub`, <4.1.3) |
| p256 | 0.14.0 (2026-07-03) | 1.85 | 1,300 | RustCrypto | none |
| ecdsa | 0.17.0 (2026-07-02) | 1.85 | 272 | RustCrypto | none |
| crypto-bigint | 0.7.11 (2026-11-08) | 1.87 | — | RustCrypto | RUSTSEC-2026-0387 (several timing leaks, <0.7.6) |
| ctutils / cmov | 0.4.8 / 0.5.4 | 1.85 | — | RustCrypto | RUSTSEC-2026-0385 (<0.4.4), RUSTSEC-2026-0003 (ARM32, <0.4.4) |

The reverse-dependency counts come from the crates.io `reverse_dependencies` endpoint. The versions measured here are not affected by any of these advisories.

**Other crates in the tree.**
- All runtime crates declare `#![no_std]`; alloc and std sit behind features.
- p256 with `ecdsa` always pulls `der` and `sec1`: p256 enables `ecdsa/der`, and elliptic-curve depends on `sec1` unconditionally.
- curve25519-dalek has a build script, whose build dependencies are `rustc_version` and `semver`. On x86_64 it also uses the `curve25519-dalek-derive` proc macro, which picks the AVX2 backend at runtime.
- With `--cfg curve25519_dalek_backend="fiat"`, the backend becomes `fiat-crypto` 0.3.0 (pure Rust, 0 `unsafe`) and the proc macro drops out. The build script stays. This compiled cleanly on 1.97 with `-D warnings`.

**Users.**
- libsignal's `Cargo.lock` has aes-gcm 0.11.1, chacha20poly1305 0.11.0, hkdf/hmac 0.13.0, sha2 0.11.0 and curve25519-dalek 5.0.0.
- Tor's `tor-llcrypto` 0.48.0 requires sha2 ^0.11.1, x25519-dalek ^3, curve25519-dalek ^5 and aes ^0.9.
- `rustls-rustcrypto` 0.1.0-pre.0 (published 2026-11-09) is RustCrypto's own rustls provider. It is still a pre-release.
- Firefox's `Cargo.lock` has sha2 0.10.8.

**Verification and audits.**
- None of these crates is formally verified, with two partial exceptions:
  - The `fiat` backend uses Coq-generated field arithmetic from fiat-crypto. It gives verified field operations only, not the curve or ladder code.
  - The curve25519-dalek README says that 280 functions of **4.1.3** that Signal uses were verified with Verus (VeriLib). That is not 5.0.0.
- NCC Group reviewed aes-gcm and chacha20poly1305 in 2020, funded by MobileCoin, with "no significant findings". That was an old version.
- NCC Group also reviewed crypto-bigint, funded by Entropy. The README adds that the implementation "has diverged significantly since the last audit".
- Quarkslab audited subtle, curve25519-dalek, and x25519-dalek ("marginal") for Tari in 2019. This comes from the Quarkslab blog as summarised by search; the page itself was not fetched.
- The p256, ecdsa and primeorder READMEs say: "never been independently audited!"

**Constant-time approach, in the crates' own words.**
- aes-gcm and chacha20poly1305: "designed to execute in constant time, either by relying on hardware intrinsics … or using a portable implementation which is only constant time on processors which implement constant-time multiplication."
- p256: constant time "using the `subtle` crate and constant-time formulas. However, it has not been thoroughly assessed to ensure that generated assembly is constant time."
- crypto-bigint: CI runs `cargo checkct`/BINSEC on x86_64 and aarch64, among other targets.
- ctutils 0.4.4+: masks "now verified on several platforms using BINSEC"; it uses cmov's `asm!` for the masks.
- curve25519-dalek: "no secret-dependent branches, no secret-dependent memory accesses … We believe that our constant-time logic is lowered to constant-time assembly, at least on x86_64."

## Family 2: formally verified pure Rust

**libcrux.**
- **Maintainer:** Cryspen. The crates.io owner team is `github:celabshq:crates`, and recent advisories link to `celabshq/libcrux`.
- **Release:** every crate is at 0.0.x, released 2026-10-07. None declares `rust-version`; the repository README states MSRV 1.89.
- **Production use:** the repository README says the library "is in pre-release … If you wish to use any of these crates in production, get in touch."
- **Reverse deps:** libcrux-sha2 16, libcrux-hmac 9, libcrux-p256 6, libcrux-chacha20poly1305 5, libcrux-curve25519 5, libcrux-hkdf 5, libcrux-ecdsa 2, libcrux-ml-kem 26.
- **Users:**
  - Firefox's `Cargo.lock` has libcrux-p256/-sha2/-hacl-rs/-traits/-macros 0.0.3. Mozilla audited exactly those versions as safe-to-deploy.
  - libsignal's `Cargo.lock` has libcrux-ml-kem 0.0.10, libcrux-hmac 0.0.8 and libcrux-sha2 0.0.8.
  - A third-party `rustls-provider-libcrux-pq` 0.1.1 exists, with 39 downloads.
  - The OpenSSH claim (ML-KEM from libcrux) is unknown: the openssh.com host was refused here.

| Crate | Version | Badge (crate README) | What the badge means (repo README) | RUSTSEC |
|---|---|---|---|---|
| libcrux-sha2 | 0.0.9 | verified-hacl | HACL* F* source verified for memory safety, functional correctness, secret independence; compiled C→safe Rust ("Compiling C to Safe Rust, Formalized"). "Top-level Rust APIs … may not be verified." | none |
| libcrux-hmac | 0.0.9 | verified-hacl | same; also pulls libcrux-sha3 (HMAC-SHA3) → libcrux-intrinsics (192 `unsafe`, SIMD), libcrux-platform | none (sha3: 3 advisories, all fixed ≤0.0.10) |
| libcrux-hkdf | 0.0.9 | verified-hacl | same; README: needs a global allocator | none |
| libcrux-chacha20poly1305 | 0.0.10 | verified-hacl | same; "XChacha20Poly1305 wrapper has not been formally verified" | RUSTSEC-2026-0124 (panic, <0.0.8); poly1305: RUSTSEC-2026-0073 (<0.0.5) |
| libcrux-curve25519 | 0.0.9 | verified-hacl | same; HACL `ecdh` returns false on all-zero output (checked in source) | none |
| libcrux-p256 | 0.0.9 | verified-hacl | same; "internal dependency … SHOULD NOT be used directly"; signing API exposed only with `expose-hacl` | none |
| libcrux-ecdsa | 0.0.9 | verified-hacl | wrapper; `sign` exists only with feature `rand` (random nonce); `Nonce` has no public constructor otherwise | none |
| libcrux-aes | 0.0.10 | **pre-verification** | not verified | predecessor libcrux-aesgcm: RUSTSEC-2026-0209/0211 (non-constant-time tag check, ≤0.0.8), renamed (0210) |
| libcrux-secrets | 0.0.7 | pre-verification | CT select/swap; `black_box` on x86_64, `asm!` csel on aarch64 | RUSTSEC-2026-0212 (aarch64 asm bug, <0.0.6) |

**Caveats from the libcrux README.**
- "executables compiled from the code in this repository are *not* verified to be side-channel resistant, although we try to enforce that the source code is secret-independent."
- No panic-freedom claim is made for the HACL-extracted Rust; such claims apply only to hax-verified crates like ML-KEM.
- libcrux-hacl-rs, the extracted code, contains 0 `unsafe`. libcrux-ecdsa and libcrux-sha3 use `#![forbid(unsafe_code)]`.

**Third-party scrutiny.**
- No named audit-firm report for libcrux was found.
- N. Kobeissi, IACR ePrint 2026/192, "Verification Theatre", reports 13 vulnerabilities in libcrux and hpke-rs. Per its abstract as summarised by search: 9 sit in unverified code, among them a missing X25519 all-zero check in hpke-rs, and 4 sit in verified code (ML-KEM and ML-DSA proof or spec errors). It also notes the P-256 ECDSA has no low-S normalisation, which TLS does not require. eprint.iacr.org and symbolic.software were refused here, so this rests on search snippets only.

**HACL\* packaging.**
- `hacl` 0.0.2 (2023) and `hacl-star` 0.1.0 (2019) are C bindings and are excluded. `hacl-rs` does not exist on crates.io.
- The pure-Rust HACL output ships as `libcrux-hacl-rs`.

**fiat-crypto 0.3.0** (2025-06-04, MSRV 1.83; MIT Coq project).
- Provides Coq-extracted, verified field arithmetic for curve25519, p224, p256 and others.
- Contains no `unsafe`, has no dependencies, and is audited by ISRG through a safe-to-deploy delta chain.
- It covers field operations only. It has no curve group law, no scalar multiplication or ladder, no ECDSA, no hashing and no AEAD.
- p256 0.14 does not use it: its field arithmetic is hand-written over crypto-bigint.
- Users include dalek's opt-in backend and orion (for X25519 and Poly1305).

## Family 3: other crates

- **graviola** 0.4.4 (2026-11-05, MSRV 1.89, by ctz, the rustls author). Excluded by the asm rule: it contains 149 `asm!` invocations, using s2n-bignum assembly that is formally proven correct, and it runs on x86_64 and aarch64 only.
  - It offers SHA-256, HMAC, HMAC-DRBG, ChaCha20-Poly1305, AES-GCM, X25519, P-256 ECDSA with RFC 6979 plus added randomness, and ML-KEM-768. A rustls provider exists, `rustls-graviola`.
  - Its tree is 4 crates. It has no audit and no RUSTSEC entry.
  - `rind` 0.3.2 is a fork of graviola.
- **orion** 0.18.1 (MSRV 1.87). `unsafe` is forbidden in the crate, but it depends on cmov, which uses asm. Its tree is 4 crates.
  - It has SHA-2, HMAC, HKDF, ChaCha20-Poly1305, X25519 and ML-KEM, but **no P-256 and no AES-GCM**.
  - README: "has **not undergone any third-party security audit**."
- **ring**: C and assembly, excluded. It has RUSTSEC-2025-0009 (AES panic) and RUSTSEC-2025-0007/0010 (unmaintained notices).
- **aws-lc-rs** 1.19.0: C (AWS-LC), excluded.
- **rsa**, as the cautionary example: RUSTSEC-2023-0071, the Marvin timing key recovery. Per the advisory, it is patched in rsa ≥0.10.0 through a move to crypto-bigint.
- **ML-KEM in pure Rust** (not measured):
  - libcrux-ml-kem 0.0.11, hax-verified. libcrux-kem offers X25519MLKEM768.
  - RustCrypto `ml-kem` 0.3.4.
  - `fips203` 0.5.0 (integritychain).
  - orion's ML-KEM.
  - graviola's ML-KEM-768 (asm).

## cargo vet: what this repo would need, and coverage today

**Setup in this repo.** The repo has no `supply-chain/` directory and no cargo vet configuration. The cargo-vet book describes what is needed:
- `cargo vet init` creates `supply-chain/{config.toml,audits.toml,imports.lock}`.
- Add `[imports.NAME] url = …` entries for the sets wanted. The registry lists actix, ariel-os, bytecode-alliance, embark-studios, fermyon, google, isrg, mozilla and zcash. Google's file aggregates the Chromium, ChromeOS, google3 and Fuchsia audits; Mozilla's aggregates Firefox, Glean and cargo-vet.
- Run `cargo vet --locked` in CI; latest cargo-vet is 0.10.3.

**Default policy.**
- By default, regular and build dependencies must be `safe-to-deploy` and dev-dependencies `safe-to-run`, checked "on all-platforms".
- To make the rule "every product crate has an imported or in-repo audit", product crates need an empty `exemptions` list in `config.toml`. Test-only tooling can stay exempt or use `[policy]` relaxations.
- Imported custom criteria, such as Google's `ub-risk-*` and `crypto-safe` or Zcash's `crypto-reviewed`, count only if they are mapped through `criteria-map`.
- Imported `[[trusted]]` publisher entries are only *suggestions* to `cargo vet suggest`. Imported wildcard audits do count.
- RustCrypto and libc versions published through crates.io Trusted Publishing have no `published_by` user, so user-id based trust and wildcard entries cannot match them.
- Whether cargo vet requires audits for the `cfg(hax)`/`cfg(valgrind_ct_test)` dependencies is unknown. Running cargo-vet here was not permitted.

**Coverage computed here.** All nine registry imports were merged, as cargo vet merges imports. Only `safe-to-deploy` was considered, with full audits, delta chains across files, and wildcard audits; trusted entries were excluded. This is an approximation of cargo vet's algorithm, not a cargo vet run.

| Set (linux tree) | Covered | Covered crates |
|---|---|---|
| A rustcrypto | 9 / 55 | chacha20 0.10.2, cpufeatures 0.3.1, num-traits, quote, rand_core, rustc_version, semver, subtle 2.6.1, zeroize 1.9.1 |
| B libcrux | 3 / 23 | quote, rand 0.10.3, rand_core |
| C mixed | 7 / 60 | cpufeatures, num-traits, quote, rand, rand_core, subtle, zeroize |
| D mixed-verified | 4 / 38 | cpufeatures, quote, rand, rand_core |
| E graviola | 0 / 4 | — (libc 0.2.191 itself is not covered: latest audited 0.2.190 by bytecode-alliance) |

None of the top-level primitive crates at the measured versions is covered. The newest audited versions in the public sets are:

| Crate | Newest audited version (set) |
|---|---|
| sha2 | 0.10.9 (isrg) |
| hmac | 0.12.1 (google, isrg) |
| hkdf | 0.12.3 (google) |
| chacha20poly1305 | 0.10.1 (zcash) |
| aes-gcm | 0.10.2 (isrg) |
| curve25519-dalek | 4.1.3 (zcash) |
| x25519-dalek | none |
| p256 | 0.11.1 (google) |
| ecdsa | 0.14.8 (google) |
| libcrux-sha2 / libcrux-p256 | 0.0.3 (mozilla) |
| other libcrux crates | none |
| graviola | none |

The current RustCrypto majors (sha2 0.11, hmac/hkdf 0.13, aead 0.6, p256 0.14) were all released in 2026 and have no public audits yet.

## Does a formally verified set cover all five primitives?

No, not completely.
- **Covered by libcrux "verified-hacl" crates:**
  - SHA-256 with HMAC and HKDF.
  - ChaCha20-Poly1305.
  - X25519, including an all-zero-output rejection.
  - The P-256 ECDSA signing core.
- **Gap 1: RFC 6979 nonce derivation.** No libcrux crate provides it. libcrux-ecdsa signs only with a random nonce, behind feature `rand`. Signing with our own nonce requires the `libcrux-p256` `expose-hacl` API (`ecdsa_sign_p256_sha2(sig, len, msg, sk, nonce)`), which the crate says "SHOULD NOT be used directly". RFC 6979's HMAC-DRBG (~40 lines) would be ours, on top of the verified HMAC; `libcrux-hmac-drbg` 0.0.2 exists but was not measured.
- **Gap 2: AES-128-GCM.** `libcrux-aes` is "pre-verification", and its predecessor had a non-constant-time tag-check advisory.
- **Caveat:** the verification applies to the HACL\* F\* source and the C→Rust translation, not to compiled binaries. libcrux's own README says binaries are not verified side-channel resistant.

## RFC 6979 determinism: pinning with A.2.5 vectors

The test in `sets/rfc6979-check` passed on 1.97.1:
- **RustCrypto:** `p256::ecdsa::SigningKey::from_slice(&x)?.sign(b"sample")` must equal the A.2.5 SHA-256 (r, s) = (EFD48B2A…3716, F7CB1C94…CDA8). This pins the whole RFC 6979 path inside `rfc6979` 0.6.0.
- **libcrux:** `libcrux_p256::ecdsa_sign_p256_sha2(&mut sig, 6, b"sample", &x, &k)` with the RFC's k = A6E3C57D…AD60 gives the same (r, s). This pins the signing core. A test of our own k-derivation would then pin k against the RFC's listed k.

## Key loading without PKCS#8/DER

- **p256 0.14 (RustCrypto).** `SigningKey::from_slice(&[u8])`/`from_bytes(&FieldBytes)` and `SecretKey::from_slice`/`from_bytes` take a raw 32-byte scalar and need no `pkcs8` feature.
  - `der`, `sec1`, `base16ct` and `const-oid` are still in the tree: they are unconditional with `ecdsa`.
  - Enabling `pem` adds **4 crates**: `pkcs8` 0.11.0, `spki` 0.8.1, `pem-rfc7468` 1.0.0 and `base64ct` 1.8.3. None has a build script or proc macro; they add +5 `unsafe` tokens (base64ct 4, pem-rfc7468 1). Total: 59 vs 55 crates.
- **libcrux.** `libcrux_ecdsa::p256::PrivateKey: TryFrom<&[u8]>`, and the raw `&[u8; 32]` arguments of the libcrux-p256 functions, take a raw scalar. The crates contain no PKCS#8 or PEM code.
- **Either way.** Extracting the scalar from a PKCS#8/SEC1 PEM file is either a small parser of our own or the RustCrypto `pem` feature.

## Ranked summary

Ranked by verified coverage first, then product tree size:
1. **D mixed-verified:** 4/5 verified (plus RFC 6979 of our own), 38 linux crates, 1 proc macro, 5 build scripts; asm via cmov (AES-GCM path) and libcrux-secrets (aarch64); no public audits for the libcrux versions; `--target all` lists cc and crabgrind.
2. **B′/B libcrux:** 4/5 verified (AES-GCM pre-verification), 21–23 linux crates, the fewest `unsafe` (313 excluding libc); no x86_64 inline asm; pre-release 0.0.x; 52 cfg-gated `--target all` crates, including C, that the current dependency check would flag.
3. **A rustcrypto:** 0/5 verified (fiat option for X25519 field arithmetic only); 55 crates; widest use (Signal, arti, rustls-rustcrypto); NCC audits on old AEAD versions only; inline asm via cmov and zeroize on both product architectures; RFC 6979 built in and pinned by test.
4. **C mixed:** 3/5 verified, 60 crates (the largest product tree), both families' asm.
5. **orion and graviola:** orion lacks P-256 and AES-GCM; graviola is excluded by the asm rule.

Other facts:
- No candidate set is free of inline asm on both architectures. libcrux is free of it on x86_64 only.
- No top-level crate in any set has an importable cargo-vet audit today.
- With the workspace's `rust-version = "1.85"`, the MSRV-aware resolver chose older `aes` 0.9.2 and `elliptic-curve` 0.14.1 (tested in `sets/msrv185`). sha2 0.11.1 (MSRV 1.88) and libcrux (1.89) exceed 1.85 anyway.

## Exact Cargo.toml per set

The common header for every set: `[package] name = "set-…", version = "0.0.0", edition = "2024", rust-version = "1.97", publish = false`. Only the `[dependencies]` differ.

```toml
# A rustcrypto  (A′ = same without aes-gcm; A+PEM = p256 features ["ecdsa","pem"])
sha2 = { version = "=0.11.1", default-features = false }
hmac = { version = "=0.13.0", default-features = false }
hkdf = { version = "=0.13.0", default-features = false }
chacha20poly1305 = { version = "=0.11.0", default-features = false }
aes-gcm = { version = "=0.11.1", default-features = false, features = ["aes"] }
x25519-dalek = { version = "=3.0.0", default-features = false, features = ["zeroize"] }
p256 = { version = "=0.14.0", default-features = false, features = ["ecdsa"] }

# B libcrux  (B′ = without libcrux-aes and libcrux-ecdsa; libcrux-p256 with features ["expose-hacl"])
libcrux-sha2 = { version = "=0.0.9", default-features = false }
libcrux-hmac = { version = "=0.0.9", default-features = false }
libcrux-hkdf = { version = "=0.0.9", default-features = false }
libcrux-chacha20poly1305 = { version = "=0.0.10", default-features = false }
libcrux-aes = { version = "=0.0.10", default-features = false }
libcrux-curve25519 = { version = "=0.0.9", default-features = false }
libcrux-p256 = { version = "=0.0.9", default-features = false }
libcrux-ecdsa = { version = "=0.0.9", default-features = false }

# C mixed
libcrux-sha2 = { version = "=0.0.9", default-features = false }
libcrux-hmac = { version = "=0.0.9", default-features = false }
libcrux-hkdf = { version = "=0.0.9", default-features = false }
libcrux-chacha20poly1305 = { version = "=0.0.10", default-features = false }
libcrux-curve25519 = { version = "=0.0.9", default-features = false }
aes-gcm = { version = "=0.11.1", default-features = false, features = ["aes"] }
p256 = { version = "=0.14.0", default-features = false, features = ["ecdsa"] }

# D mixed-verified
libcrux-sha2 = { version = "=0.0.9", default-features = false }
libcrux-hmac = { version = "=0.0.9", default-features = false }
libcrux-hkdf = { version = "=0.0.9", default-features = false }
libcrux-chacha20poly1305 = { version = "=0.0.10", default-features = false }
libcrux-curve25519 = { version = "=0.0.9", default-features = false }
libcrux-p256 = { version = "=0.0.9", default-features = false, features = ["expose-hacl"] }
aes-gcm = { version = "=0.11.1", default-features = false, features = ["aes"] }

# E graviola
graviola = { version = "=0.4.4", default-features = false }
# F orion
orion = { version = "=0.18.1", default-features = false }
```

## Full crate lists (linux product targets)

| Set | Crates |
|---|---|
| A rustcrypto (55) | aead 0.6.1, aes 0.9.4, aes-gcm 0.11.1, autocfg 1.5.1, base16ct 1.0.0, block-buffer 0.12.1, cfg-if 1.0.5, chacha20 0.10.2, chacha20poly1305 0.11.0, cipher 0.5.2, cmov 0.5.4, const-oid 0.10.2, cpubits 0.1.1, cpufeatures 0.3.1, crypto-bigint 0.7.11, crypto-common 0.2.2, ctr 0.10.1, ctutils 0.4.8, curve25519-dalek 5.0.0, curve25519-dalek-derive 0.1.1 (pm), der 0.8.3, digest 0.11.3, ecdsa 0.17.0, elliptic-curve 0.14.3, ff 0.14.0, ghash 0.6.0, group 0.14.0, hkdf 0.13.0, hmac 0.13.0, hybrid-array 0.4.15, inout 0.2.6, libc 0.2.191, num-traits 0.2.19, p256 0.14.0, poly1305 0.9.1, polyval 0.7.3, primefield 0.14.0, primeorder 0.14.0, proc-macro2 1.0.107, quote 1.0.47, rand_core 0.10.1, rfc6979 0.6.0, rustc_version 0.4.1, sec1 0.8.1, semver 1.0.28, sha2 0.11.1, signature 3.0.0, subtle 2.6.1, syn 2.0.119, typenum 1.20.1, unicode-ident 1.0.27, universal-hash 0.6.1, wnaf 0.14.1, x25519-dalek 3.0.0, zeroize 1.9.1. `--target all` adds fiat-crypto 0.3.0. |
| B libcrux (23) | libc 0.2.191, libcrux-aes 0.0.10, libcrux-chacha20poly1305 0.0.10, libcrux-curve25519 0.0.9, libcrux-ecdsa 0.0.9, libcrux-hacl-rs 0.0.6, libcrux-hkdf 0.0.9, libcrux-hmac 0.0.9, libcrux-intrinsics 0.0.9, libcrux-macros 0.0.4 (pm), libcrux-p256 0.0.9, libcrux-platform 0.0.4, libcrux-poly1305 0.0.7, libcrux-secrets 0.0.7, libcrux-sha2 0.0.9, libcrux-sha3 0.0.11, libcrux-traits 0.0.9, proc-macro2 1.0.107, quote 1.0.47, rand 0.10.3, rand_core 0.10.1, syn 3.0.10, unicode-ident 1.0.27. `--target all` adds 52 crates: autocfg, bindgen, bitflags, cc, cexpr, cfg-if, chacha20, clang-sys, core-models, cpufeatures, crabgrind, either, find-msvc-tools, getrandom, glob, hax-lib ×2, hax-lib-macros ×2 (pm), hax-lib-macros-types ×2, itertools, itoa, libloading, log, memchr, minimal-lexical, nom, num-bigint, num-integer, num-traits, pastey (pm), pkg-config, prettyplease, proc-macro-error2, proc-macro-error-attr2 (pm), r-efi, regex, regex-automata, regex-syntax, rustc-hash, rustversion (pm), serde, serde_core, serde_derive (pm), serde_json, shlex ×2, syn 2, uuid, windows-link, zmij. |
| D mixed-verified (38) | aead, aes, aes-gcm, block-buffer, cipher, cmov, cpubits, cpufeatures, crypto-common, ctr, ctutils, ghash, hybrid-array, inout, libc, libcrux-chacha20poly1305, libcrux-curve25519, libcrux-hacl-rs, libcrux-hkdf, libcrux-hmac, libcrux-intrinsics, libcrux-macros (pm), libcrux-p256, libcrux-platform, libcrux-poly1305, libcrux-secrets, libcrux-sha2, libcrux-sha3, libcrux-traits, polyval, proc-macro2, quote, rand, rand_core, syn 3, typenum, unicode-ident, universal-hash (versions as in A and B) |
| C mixed (60) | union of A's list minus {chacha20, chacha20poly1305, curve25519-dalek, curve25519-dalek-derive, hkdf, poly1305, rustc_version, semver, syn 2, x25519-dalek} (45) and B's list minus {libcrux-aes, libcrux-ecdsa, libcrux-p256} (20); 5 shared (libc, proc-macro2, quote, rand_core, unicode-ident) |
| E graviola (4) | cfg-if, getrandom 0.4.3, graviola 0.4.4, libc |
| F orion (4) | cmov 0.5.4, fiat-crypto 0.3.0, orion 0.18.1, subtle 2.6.1 |

`unsafe` counts per crate (linux): aes 60, base16ct 4, base64ct 4, block-buffer 20, chacha20 47, cmov 25 (7 `asm!`), const-oid 1, cpufeatures 11, crypto-bigint 14, curve25519-dalek 33, curve25519-dalek-derive 5, der 4, elliptic-curve 2, getrandom 109, graviola 343, hybrid-array 37, inout 30, libc 731, libcrux-intrinsics 192, libcrux-platform 2, libcrux-secrets 18 (4 `asm!`), libcrux-traits 4, num-traits 1, pem-rfc7468 1, poly1305 50, polyval 24, proc-macro2 4, rand 14, semver 47, sha2 39 (6 `asm!`, riscv/loongarch), subtle 2, syn 2.x 67 / 3.x 77, unicode-ident 2, zeroize 17 (1 `asm!`). All other crates: 0.

## Sources

- crates.io API: `https://crates.io/api/v1/crates/<name>` and `/reverse_dependencies`, `/owner_user`, `/owner_team`, `/<ver>/dependencies` for every crate named above (fetched 2026-11-10)
- libcrux README: https://raw.githubusercontent.com/cryspen/libcrux/main/Readme.md
- libcrux crate READMEs: https://raw.githubusercontent.com/cryspen/libcrux/main/crates/algorithms/{sha2,hmac,hkdf,chacha20poly1305,poly1305,curve25519,p256,ecdsa}/Readme.md, …/aes/README.md, https://raw.githubusercontent.com/cryspen/libcrux/main/libcrux-ml-kem/README.md
- RustSec advisory DB (clone at d9cb059, 2026-11-09): https://github.com/rustsec/advisory-db (crates/sha2, chacha20, aes-gcm, curve25519-dalek, crypto-bigint, cmov, ctutils, libcrux-*, rsa, ring, rand, rand_core)
- cargo-vet registry: https://raw.githubusercontent.com/mozilla/cargo-vet/main/registry.toml
- Audit sets: https://raw.githubusercontent.com/{actix/supply-chain,ariel-os/ariel-os,bytecode-alliance/wasmtime,EmbarkStudios/rust-ecosystem,fermyon/spin,google/supply-chain,divviup/libprio-rs,mozilla/supply-chain,zcash/rust-ecosystem}/…/audits.toml (paths as in registry.toml); https://raw.githubusercontent.com/mozilla-firefox/firefox/main/supply-chain/audits.toml; https://raw.githubusercontent.com/chromium/chromium/main/third_party/rust/chromium_crates_io/supply-chain/audits.toml (chromium.googlesource.com was refused by the proxy; Google's aggregate covers it)
- cargo-vet book sources: https://raw.githubusercontent.com/mozilla/cargo-vet/main/book/src/{importing-audits,trusting-publishers,trusted-entries,wildcard-audit-entries,specifying-policies,configuring-ci,config}.md (mozilla.github.io was refused)
- Firefox Cargo.lock: https://raw.githubusercontent.com/mozilla-firefox/firefox/main/Cargo.lock
- libsignal Cargo.lock: https://raw.githubusercontent.com/signalapp/libsignal/main/Cargo.lock
- Crate READMEs read from the downloaded sources: aes-gcm 0.11.1, chacha20poly1305 0.11.0, p256 0.14.0, ecdsa 0.17.0, primeorder 0.14.0, crypto-bigint 0.7.11, subtle 2.6.1, curve25519-dalek 5.0.0, graviola 0.4.4, orion 0.18.1, fiat-crypto 0.3.0
- NCC Group AEAD review (2020): https://web.archive.org/web/20200606025324/https://research.nccgroup.com/2020/02/26/public-report-rustcrypto-aes-gcm-and-chacha20poly1305-implementation-review/
- NCC Group crypto-bigint review: https://www.nccgroup.com/us/research-blog/public-report-entropyrust-cryptography-review/
- Quarkslab dalek audit (2019; seen through search only): https://blog.quarkslab.com/security-audit-of-dalek-libraries.html
- curve25519-dalek Verus verification: https://verilib.org/cert/5132 (cited from the README)
- "Verification Theatre", ePrint 2026/192 (seen through search only; host refused): https://eprint.iacr.org/2026/192 ; https://www.symbolic.software/blog/2026-02-12-cryspen-response
- RFC 6979 A.2.5 (vector values used in `sets/rfc6979-check`): https://www.rfc-editor.org/rfc/rfc6979#appendix-A.2.5
- Hosts refused by the proxy: api.osv.dev, chromium.googlesource.com, mozilla.github.io, eprint.iacr.org, www.symbolic.software, blog.quarkslab.com, www.openssh.com; api.github.com for unattached repos
