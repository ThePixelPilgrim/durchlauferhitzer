# TLS helper spike — can a separate rustls helper carry the handshake?

2026-11-18, a throwaway spike. The code is on the scratch branch `spike-tls-helper` at `d802bff`, which is never to be merged. It ran in the cloud container: x86_64, 4 vCPU, as root, over loopback only. All numbers come from this one shared machine, where the clients and the server shared the same cores; the clients were `curl` 8.5 and `openssl` 3.0, plus a load client of the spike's own.

**Recommendation: the design works end to end in pure Rust.** It costs CPU per handshake and bulk throughput.

What worked:

- a rustls handshake in a one-shot helper, running under its own user and a seccomp filter;
- the signature delegated to a key process, so the helper never holds the private key;
- a 191-line record layer in the server, which served `curl` and `openssl s_client`, KeyUpdate included;
- the helper replaced on disk with `rename` under load, without one failed handshake.

What it costs, as medians on this machine:

| Cost | Without | With the helper |
|---|---|---|
| CPU per handshake | 360 µs in-process | 1164 µs musl, 1872 µs glibc |
| Handshakes/s | 4596 in-process | 2023 musl, 1340 glibc |
| Bulk MiB/s | 2114 `sendfile` | 990 AES-GCM, 542 ChaCha20 |

## 1. What this means for the design

This is the controller's reading, for the owner to decide; nothing here is decided. The tags give the evidence:

- `(measured)`: a number from a spike run;
- `(observed)`: a behaviour seen in a spike run;
- `(read from source)`: read in a crate's source or README;
- `(not settled)`: the spike did not answer it.

1. **rustls runs without C, over a provider of our own.** rustls 0.23.46 with default features off worked with a hand-written provider of 302 code lines over the primitives the project already vetted. That provider is our own glue, and nobody has audited it. `rustls-rustcrypto` 0.1.0-pre.0 works too, but its README says "DO NOT USE THIS IN PRODUCTION", and it brings more crates. `(measured; read from source)`

   | Helper variant | External crates |
   |---|---|
   | Own provider, ChaCha20 and AES-GCM | 43 |
   | Own provider, ChaCha20 only, `fiat` backend | 32 |
   | `rustls-rustcrypto` | 75 |

2. **The hand-over is safe only if the helper feeds rustls one record at a time.** A client may send its first request in the same write as its Finished. A naive 16 KiB read swallowed that request inside rustls, which dropped it without an error, and 0 of 252 connections succeeded. This must become a test. `(measured)`
3. **The record layer in the server is small, but it brings crypto crates into the server.** `server/src/record.rs` has 191 code lines and covers ChaCha20-Poly1305, AES-128-GCM, close_notify, alerts and KeyUpdate. The spike server's manifest names `chacha20poly1305`, `aes-gcm`, `hmac` and `sha2` for it; the results file gives no count of their tree. Today the server's tree holds `libc` only. `(measured; read from source)`
   - With kTLS as the record mode, the server could stay at `libc` only.
   - Honouring a KeyUpdate under kTLS still needs one HKDF step in the server, so `libc` only holds if KeyUpdate closes the connection. `(not settled)`
   - This trade-off is behind the default-mode decision.
4. **Leaving `sendfile` costs throughput.** One connection fetched 1000 MiB over loopback, 3 runs each. The client, `curl`, used about 60 % of the wall time on its own core. `(measured)`

   | Path | MiB/s | Server CPU s per 1000 MiB |
   |---|---|---|
   | Plain, `sendfile` | 1994–2177 | 0.44–0.49 |
   | ChaCha20-Poly1305, AVX2 | 540–562 | 1.75–1.82 |
   | AES-128-GCM, AES-NI and CLMUL | 971–1028 | 0.95–1.01 |

5. **A helper per connection costs CPU.** The table gives medians over 3 runs with 4 client threads. The uid switch adds nothing measurable: 1872 µs with it, 1869 µs without. `(measured)`

   | Model | CPU µs | HS/s | p50 ms |
   |---|---|---|---|
   | In-process | 360 | 4596 | 0.83 |
   | fork+exec, glibc | 1872 | 1340 | 2.78 |
   | fork+exec, static musl | 1164 | 2023 | 1.90 |
   | Pool of 8, glibc | 1873 | 1539 | 2.49 |
   | Pool of 8, static musl | 1198, 1246 | 1999 | 1.93 |

   - The pool saves no CPU.
   - It buys latency for glibc: p50 from 2.01 to 1.13 ms with one client thread.
   - It buys almost nothing for static musl: p50 from 1.36 to 1.24 ms.
   - Static musl without a pool beats glibc with one, in CPU and in rate.
6. **KeyUpdate needs the traffic secret, which the supported export does not give.** `dangerous_extract_secrets()` returns key, IV and sequence number only. The spike took the secret from rustls's `KeyLog` hook, a debugging interface. The consequence to decide: rely on that hook, or close a connection on a client KeyUpdate in version 1. `(observed; read from source)`
7. **The key process is a signing oracle for the helper's user.** It signs any 32-byte transcript hash inside a well-formed CertificateVerify content. `(observed)`
   - A compromised helper can obtain a signature for any handshake it relays, and so impersonate the server while the compromise lasts.
   - It cannot read the key, and it cannot get a signature over anything but a TLS 1.3 server CertificateVerify.
   - The limit the design can enforce: one signature per helper process, with the key process building the CertificateVerify content itself. `(not settled)`
8. **Update without restart works with `rename` and fails with an in-place overwrite.** So the packages must replace the helper atomically, which package managers do. `(measured)`
   - With `rename`, 0 handshakes failed, and the last old-helper handshake came 2 to 16 ms after the rename.
   - An in-place `cp` hit "Text file busy" and failed 4 to 6 handshakes per run.
   - With a pool, an in-place overwrite never succeeded.
9. **Separate users work, with two findings that shape the interface.** Three uids worked, and every cross-uid check rejected the wrong peer. A spawner started directly under the helper's user, without root, worked at the same cost. `(observed; measured)`
   - `SO_PEERCRED` on a socketpair reports the pair's creator. So the per-message `SCM_CREDENTIALS` is the check that works.
   - Helpers share a uid and could ptrace each other. So the helper must be non-dumpable, in addition to its seccomp filter.
   - Different uids cannot signal each other, so the server cannot kill a hung helper.
   - A root spawner can kill it. An unprivileged spawner can too, because it shares the helper's uid.
10. **What the spike did not try:** `(not settled)`
    - Landlock and namespaces for the helper;
    - a seccomp filter installed before exec;
    - session tickets and resumption;
    - kTLS as the record mode, because this container has no kTLS;
    - a real static musl build through the repo's own config.

## 2. Results per question

### Question 1: rustls without C

rustls 0.23.46 works with a hand-written pure-Rust provider of 302 code lines. `rustls-rustcrypto` also works, but its README warns against production use.

rustls itself:

- 0.23.46, released 2026-10-14, `rust-version = 1.71`; the toolchain was rustc 1.97.1.
- 0.24.0-dev.3 (2026-11-12, MSRV 1.85) is a pre-release and was not used.
- Features: `default-features = false, features = ["std"]`. Neither `ring` nor `aws_lc_rs` enters the tree.

The hand-written provider, `provider/src/lib.rs`, 349 lines, 302 code lines:

- TLS 1.3 only: `TLS13_CHACHA20_POLY1305_SHA256`, and `TLS13_AES_128_GCM_SHA256` behind the default feature `aes`;
- X25519 through `x25519_dalek::x25519`;
- SHA-256 and HMAC; HKDF through rustls's own `HkdfUsingHmac`, so no `hkdf` crate;
- randomness from `getrandom(2)` through `libc`, so no `getrandom` crate;
- an empty signature-verification list, which rustls accepted for a server without client auth;
- a `KeyProvider` that always errors, because signing is delegated;
- no `p256`: ECDSA lives only in the key process.

What was awkward:

- the `aead` 0.6 generics, which need an `AeadInOut<NonceSize = U12, TagSize = U16>` bound to convert nonces and tags;
- `CryptoProvider` insists on a `KeyProvider` even when none is used;
- key set-up returns `Result` for lengths rustls already guarantees.

It worked first time against `curl`, `openssl s_client` and rustls with ring. OpenSSL and `curl` negotiated ChaCha20; rustls with ring negotiated AES-128-GCM.

`rustls-rustcrypto` 0.1.0-pre.0:

- released 2026-11-09; the previous release was 0.0.2-alpha in 2024;
- MSRV 1.87; owners `tarcieri` and the team `rustcrypto:rustls`;
- README: "⚠️USE THIS AT YOUR OWN RISK! DO NOT USE THIS IN PRODUCTION⚠️";
- the README says nothing about audits;
- it builds only with feature `std`;
- it offers TLS 1.2 and 1.3, X25519, P-256 and P-384, and RSA, ECDSA and Ed25519.

### Question 2: dependency tree

The helper with our provider has 43 external crates, and 32 with ChaCha20 only and the `fiat` backend. None has C or assembly files, and none uses `cc` or `cmake`.

Counted with `cargo tree -e normal,build --target all`:

| Variant | Entries | External |
|---|---|---|
| Own provider, default | 45 | 43 |
| ChaCha only, x86_64 | 38 | 36 |
| ChaCha only, `fiat` | 34 | 32 |
| `rustls-rustcrypto` | 76 | 75 |

- The `fiat` helper served handshakes: 869 ok, 0 failed.
- `rustls-rustcrypto` adds 32 crates, among them `rsa`, `p384`, `ed25519-dalek` and the PKCS stack.
- A `find` for C, C++ and assembly sources over both trees returned 0.
- Build scripts: `rustls` (one nightly cfg), `curve25519-dalek` (backend choice), `libc` (cfgs). `proc-macro2` and `quote` have them too, but leave with the `fiat` backend.
- `rustls-rustcrypto` adds the build scripts of `getrandom` 0.4.3, `num-traits` and `paste`.

Inline `asm!` compiled on x86_64-linux:

- `cmov` 0.5.4, two macro sites, reached through `ctutils`;
- `zeroize` 1.9.2, one site holding only a comment.

That matches the allowance in NFR-MAINT-01. Other `asm!` in the tree is gated to other architectures.

`scripts/check-product-deps.sh` would reject every crate but `libc`. With open decision 5's crates added to its allow-list, these still remain outside it:

- `rustls`, `rustls-webpki`, `rustls-pki-types`, `untrusted`;
- `once_cell`, `subtle`, `cmov`, `ctutils`, `zeroize`, `cpufeatures`, `fiat-crypto`;
- the RustCrypto plumbing: `aead`, `cipher`, `digest`, `crypto-common`, `hybrid-array`, `typenum`, `inout`, `block-buffer`, `universal-hash`, `rand_core`, `cfg-if`;
- with AES on: `aes`, `aes-gcm`, `ctr`, `ghash`, `polyval`.

Lines with `unsafe`, the largest: `libc` 810, `syn` 76, `chacha20` 66, `aes` 60, `once_cell` 51, `poly1305` 50. `rustls` has 5; `rustls-webpki`, `x25519-dalek`, `hmac`, `digest`, `aead` and `fiat-crypto` have 0.

### Question 3: helper binary size

The helper is about 0.8 MB stripped, whichever libc. The build used `lto = true`, `codegen-units = 1` and `panic = "abort"`.

| Build | Stripped | Linking |
|---|---|---|
| glibc, dynamic | 790,672 B | `libc.so.6`, `libgcc_s.so.1` |
| musl, static | 877,104 B | static-pie |
| glibc, static | 1,659,808 B | static-pie |

- Unstripped: 944,056 B for glibc, 1,043,840 B for musl.
- The repo's `.cargo/config.toml` sets `-crt-static` for musl. With no musl linker here, that build silently produced a binary linked against glibc.
- The static musl numbers come from overriding `RUSTFLAGS` with `+crt-static`.

### Question 4: a working helper

The handshake completes with `curl` and with `openssl s_client -tls1_3`, and TLS 1.2 is refused.

- Configuration: TLS 1.3 only, no tickets, no early data, no client auth, ALPN `http/1.1`.
- Certificate: one ECDSA P-256 leaf under a throwaway CA. webpki rejected a self-signed leaf with `CaUsedAsEndEntity`.
- `curl` negotiated `TLS_CHACHA20_POLY1305_SHA256` with X25519 and exited 0.
- `openssl s_client -tls1_2` got alert 70, `protocol version`.

### Question 5: secret export

`dangerous_extract_secrets()` works with a custom provider, but returns key, IV and sequence number only, not the traffic secret. The secret came from the `KeyLog` hook.

- It returns `ExtractedSecrets { tx: (u64, ConnectionTrafficSecrets), rx: (u64, ConnectionTrafficSecrets) }`.
- The variant (`Aes128Gcm`, `Aes256Gcm` or `Chacha20Poly1305`) comes from the provider's own `extract_keys`.
- Observed sequence numbers: tx 0 and rx 0 on every connection.
- Preconditions (`conn.rs:1225-1248`): extraction enabled, handshake complete, no TLS records buffered to send.
- rustls does not check for decrypted, unread plaintext, which is lost silently (question 6).
- The `KeyLog` secrets were checked against the extracted key and IV on every connection. No mismatch occurred in more than 300,000 handshakes.

What rustls says about later messages:

- The unbuffered API's copy of the call is deprecated: it "does not support session tickets or key updates".
- The supported route, `dangerous_into_kernel_connection()`, returns a `KernelConnection` that computes KeyUpdate secrets.
- That object lives in the handshake process. The helper exits, so it cannot serve.

### Question 6: record-exact reading

The helper reads one record at a time and stops after the client's Finished, so the client's request stays in the socket for the server. A naive read loses it.

- The coalescing client sent CCS, Finished and the request in one `writev` of 140 bytes: 6 + 58 + 76.
- The helper read 5 then 238 bytes (ClientHello), 5 then 1 (CCS), 5 then 53 (Finished), and stopped.
- Every connection's request then reached the server as the first record, at rx seq 0.
- A probe of `conn.reader()` after the handshake saw 0 bytes in all runs.

With `SPIKE_NAIVE_READ=1`, one read of up to 16 KiB:

- the helper logged 54 bytes of client plaintext already decrypted inside it;
- the load client reported `ok=0 fail=252`.

A header that is not TLS returns at once without waiting for a body. Content types outside 20 to 23, or a major version other than 3, count as not TLS.

### Question 7: record layer in the server

It works: `curl` exits 0, and `openssl s_client` survives both kinds of KeyUpdate. `server/src/record.rs` has 219 lines, 191 of them code.

- The server gets the socket by `SCM_RIGHTS` from the helper, with a fixed 226-byte hand-over message.
- Covered: nonce and AAD, inner content type with padding stripped, CCS skipped, close_notify answered, other alerts fatal.
- An unexpected post-handshake handshake message gets `unexpected_message`.
- KeyUpdate is about 15 lines: `HKDF-Expand-Label(secret, "traffic upd", "", 32)`, a fresh key and IV, and seq 0.
- The run: request, `K`, request, `k`, request. All three got 200, with ChaCha20 and with AES-128-GCM.

### Question 8: delegated signing

It works, and the helper never holds the key. The helper's `Signer::sign` sends the content over a pre-connected `SOCK_SEQPACKET` socket to `keyd`, uid 2003.

| Check | Result |
|---|---|
| Private scalar in helper memory | 0 times in 1,077,248 bytes |
| Private scalar in `keyd` memory | 15 times |
| Helper reads the key file | `Permission denied` |
| `keyd` gets 34 arbitrary bytes | rejected, connection closed |

- rustls asks to sign the full 130-byte content: 64 × `0x20`, `"TLS 1.3, server CertificateVerify"`, `0x00`, and the 32-byte transcript hash.
- `keyd` hashes it with SHA-256 and returns a DER signature, 70 bytes in the cases seen.
- The signatures are deterministic, RFC 6979 (`ecdsa-0.17.0/src/signing.rs:172,185`).
- The hash is opaque, so a compromised helper can still get a signature over any transcript.

### Question 9: cost per handshake

In-process, the server spends 360 µs of CPU per handshake; with a fork+exec helper, 1164 µs (static musl) to 1872 µs (glibc). A pool cuts latency only for glibc and saves no CPU.

Method: the spike's load client, 4 threads, 10 s, a new connection per request, 3 runs. Server CPU covers the server, the spawner with its reaped helpers, and `keyd`. Values are median, then min–max.

| Model | HS/s | CPU µs | p50 ms |
|---|---|---|---|
| In-process, local key | 4596 (4524–4741) | 360 (349–361) | 0.83 |
| In-process, key in `keyd` | 4263 (4036–4350) | 451 (442–477) | 0.90 |
| fork+exec, glibc | 1340 (1335–1371) | 1872 (1842–1910) | 2.78 |
| fork+exec, static musl | 2023 (1918–2063) | 1164 (1141–1195) | 1.90 |
| Pool of 8, glibc | 1539 (1492–1550) | 1873 (1854–1928) | 2.49 |
| Pool of 8, static musl | 1999 (1932–2016) | 1198, 1246 | 1.93 |
| Unprivileged spawner, glibc | 1366 (1353–1372) | 1869 (1861–1878) | 2.74 |
| Same, pool of 8 | 1532 (1519–1561) | 1865 (1843–1880) | 2.48 |

One client thread, one 5 s run each:

| Model | p50 ms | p99 ms |
|---|---|---|
| In-process | 0.67 | 1.14 |
| fork+exec, glibc | 2.01 | 3.62 |
| fork+exec, static glibc | 1.89 | 3.08 |
| fork+exec, static musl | 1.36 | 2.65 |
| Pool, glibc | 1.13 | 2.36 |
| Pool, musl | 1.24 | 1.73 |

The helper's start, from its own clock, over 2,200 to 3,500 helpers each:

| Phase | glibc | static glibc | static musl |
|---|---|---|---|
| fork+exec to `main`, median | 983 µs | 788 µs | 268 µs |
| same, p90 | 1274 µs | 1021 µs | 340 µs |
| Handshake | 723–808 µs | 778 µs | 771 µs |
| Peak RSS | 2.5 MiB | 1.46 MiB | 0.83 MiB |

- Reading the chain from the memfd and building the config took 7 µs each.
- The difference between models is almost all exec and dynamic linking.
- The uid switch adds nothing: 1872 µs and an exec-to-main median of 1051 µs, against 1869 µs and 1059 µs without.
- Runs agreed within ±3 % for rate and ±4 % for CPU.
- Two runs with a restarted `keyd` gave invalid CPU numbers and are excluded.
- The client used 270 to 350 µs of CPU per connection on the same 4 cores.

### Question 10: system calls and containment

After start-up the helper needs 13 distinct system calls plus `mmap`, and the handshake works under a kill-on-anything-else seccomp filter. rustls and the provider issue only `getrandom`.

What was applied to each helper:

- seccomp: 22 allowed calls, `SECCOMP_RET_KILL_PROCESS` for any other, installed after the config is built;
- uid and gid 2002, no supplementary groups, `NO_NEW_PRIVS`, no capabilities;
- the certificate chain through an inherited memfd, closed after reading;
- the `keyd` connection made after the uid switch.

What `/proc` showed for a pooled helper: `CapEff` 0, `NoNewPrivs` 1, `Seccomp` 2, and descriptors 0 to 4 only.

| Result | glibc | static musl |
|---|---|---|
| Handshakes under seccomp | 3,924 ok, 0 failed | 5,895 ok, 0 failed |
| `openat` at start-up | 4 | none |

- A helper reading the key file under seccomp died with SIGSYS, signal 31.
- glibc's start-up needs `openat` for `ld.so` before a filter can be installed.
- The static musl helper opens no file, so its filter could be installed before exec. That was not tried.

### Question 11: update without restart

`rename` is seamless; an in-place `cp` fails handshakes. Each run had a 6 s load loop with 4 client threads, and the replacement started at about 2.0 s.

| Run | Failed | Last old helper after |
|---|---|---|
| `rename`, fork+exec, musl, 3 runs | 0, 0, 0 | 2–4 ms |
| `rename`, pool of 8, musl | 0 | about 10 ms |
| `rename`, pool of 8, glibc | 0 | about 16 ms |
| `cp`, fork+exec, musl, 3 runs | 4, 6, 5 of ~12,000 | 25–50 ms |
| `cp`, fork+exec, glibc | 6 of 6,853 | about 0.8 s |
| `cp`, pool of 8, musl | `cp` never succeeded | never switched |

- `cp` first failed with "Text file busy" while a helper executed the file: 6, 13 and 7 times for musl, 199 times for glibc.
- Helpers forked during `cp`'s write failed `execve` and exited 127; their clients saw a reset.
- With a pool, `cp` got 2,443 "Text file busy" errors over 7.3 s, and all 10,649 handshakes used the old helper.
- No partly written file was ever executed: the kernel refuses `execve` while the file is open for writing.
- With a pool, the 8 pre-started old helpers still served after the rename, until they drained.

### Question 12: failure handling

In every failure case the server sees the same thing: EOF on its control channel with no message. It logs one line and does nothing else, because the socket closes when the helper dies.

| Case | Client saw |
|---|---|
| `kill -9` of the helper mid-handshake | 739 bytes, then EOF |
| Silent client | EOF after 10.14 s, the helper's own timeout |
| Plain HTTP request | `decode_error` alert, then RST |
| Valid header, garbage body | EOF, no alert |
| Client aborts mid-ClientHello | – |
| TLS 1.2 client | alert 70 |

- The spawner's 15 s SIGKILL backstop was never needed.
- The RST comes from the helper exiting with unread client bytes.
- Why the garbage body got no alert was not investigated.

### Question 13: the cost of leaving `sendfile`

`sendfile` serves about 2,100 MiB/s; the record layer serves about 1,000 MiB/s with AES-128-GCM and about 550 MiB/s with ChaCha20-Poly1305. Each run fetched a 100 MiB file 10 times over one keep-alive connection, 3 runs each.

| Path | MiB/s | Server s | `curl` s |
|---|---|---|---|
| Plain, `sendfile` | 2177, 1994, 2114 | 0.44–0.49 | 0.30–0.31 |
| ChaCha20, AVX2 | 540, 562, 542 | 1.75–1.82 | 1.03–1.13 |
| AES-128-GCM | 990, 971, 1028 | 0.95–1.01 | 0.58–0.60 |
| ChaCha20, `soft` | 339, 315, 290 | 2.91–3.41 | 1.03–1.29 |

CPU seconds are per 1000 MiB. Records were 16 KiB: `pread`, encrypt, one `send` each.

- The server is the bottleneck in every TLS case: its CPU roughly equals the wall time.
- `curl` used about 60 % of the wall time on its own core.
- `chacha20` and `poly1305` pick AVX2 at run time; AVX-512 needs an opt-in cfg.
- `aes` uses AES-NI at run time; VAES needs a cfg.
- Forcing the `soft` backends dropped ChaCha20 from about 550 to about 315 MiB/s, which proves the SIMD paths were active.

### Question 14: separate users

Three uids work: server 2001, helper 2002, key process 2003. Every cross-uid check rejected the wrong peer, and `SCM_RIGHTS` carries the client socket across uids.

| Check | Mechanism | Wrong peer |
|---|---|---|
| Server takes results from 2002 only | `SCM_CREDENTIALS` per message | 2003 rejected |
| `keyd` signs for 2002 only | `SO_PEERCRED` at `accept` | 2001 and 0 rejected |
| Helper takes work from its spawner only | `SO_PEERCRED` of the inbox | 2003 rejected |
| Unprivileged spawner serves 2001 only | `SO_PEERCRED` at `accept` | 2003 rejected |

- `SO_PEERCRED` cannot identify the helper on a socketpair: it reports whoever created the pair.
- A run with the uid switch off failed more than 18,000 handshakes at `keyd`.
- The server kept using the socket after the helper exited, and also while it was still alive.

Who can reach whom, tested with `setpriv`; this kernel has no Yama:

| Actor | Target | Result |
|---|---|---|
| Helper | server, `keyd`, root spawner | no kill, no ptrace |
| Helper | key file | `Permission denied` |
| Server | helper | no kill, no ptrace |
| Helper | another helper | kill and ptrace work |
| Helper | a non-dumpable helper | no ptrace, no `/proc/pid/mem` |

Who kills a hung helper:

- the helper itself, through its 10 s `SO_RCVTIMEO`;
- the spawner, which SIGKILLs any helper older than 15 s. A root spawner can, and so can an unprivileged one, which shares the helper's uid.

The unprivileged spawner ran as uid 2002, as an init system would start it. It listens on an abstract unix socket and serves only uid 2001. It cost the same as the root spawner: 1869 against 1872 µs, 1366 against 1340 handshakes/s. A compromised helper could ptrace it unless both are non-dumpable; seccomp blocks that too.

## 3. Not settled

- **The signing oracle:** `keyd` signs any transcript hash in a well-formed content, so a compromised helper can get signatures for handshakes it relays.
- **`keyd` keeps 15 copies of the private scalar** and zeroizes none. That measures the naive spike, not the idea.
- **KeyUpdate needs the traffic secret:** the spike takes it from `KeyLog`, a debugging hook.
- **rustls drops buffered plaintext silently** on extraction; only the record-exact reader protects against it.
- **No alert on a well-formed but oversized handshake header:** the client saw only EOF.
- **The musl build:** the repo's config produced a glibc-linked binary here. Static musl needs `+crt-static`, which contradicts NFR-PORT-02's dynamic-musl rule. Dynamic musl was not measured.
- **No Landlock and no namespaces** for the helper.
- **No seccomp filter before exec.**
- **Zombies for up to 1 s:** the spawner reaps only at dispatch or on a 1 s tick. A real spawner would use SIGCHLD or a pidfd.
- **No tickets or resumption:** `KernelConnection` does not cover the server side.
- **Coarse CPU numbers:** 10 ms ticks over 13,000 to 47,000 handshakes per run, on cores shared with the client.
- **Not the real server:** one thread per connection, not the epoll server, and a pool refilled synchronously.
- **Loopback only** for the bulk comparison; the client's decryption competed for the same machine.
- **Inconsistencies inside the results file:**
  - Its summary gives server CPU "per GiB", but its table measures per 1000 MiB.
  - It says every handshake after the rename used the new helper, but with a pool the 8 old helpers served for up to 16 ms.
  - It says a pool cuts latency "about in half". That holds for glibc with one client thread, not for musl or under load.

Spike bugs fixed along the way, not problems with the idea:

- Nagle and delayed ACK added a 40 ms tail until `TCP_NODELAY` was set.
- The first update runs wrongly had a pool of 8.
- The first CPU accounting picked up stray pids.

## 4. Method

The spike was its own workspace with its own `Cargo.lock`, excluded from the root workspace.

- **Code:** `spike/tls-helper/` on `spike-tls-helper` at `d802bff`.
- **Binaries:** the helper `durchlauferhitzer-tls`, the provider, `keyd`, the server with its spawners and record layer, the `rustls-rustcrypto` comparison, and the load client.
- **Load client:** rustls with ring, in its own workspace, test-only. It reports rates, latency percentiles and its own CPU, and it can coalesce the request with Finished.
- **Machine:** 4 vCPU Intel Xeon at 2.30 GHz with AVX2, AVX-512F, AES-NI, VAES and SHA-NI, kernel 6.18.44, rustc 1.97.1.
- **Measuring:** server CPU from `/proc/<pid>/stat`, the helper's start from its own `CLOCK_MONOTONIC` stamps, system calls from `strace -f -ff`.
- **Users:** uids 2001, 2002 and 2003, with a root spawner by default and an unprivileged spawner as the variant.

Crate versions from the spike's `Cargo.lock`:

| Crate | Version |
|---|---|
| `rustls` | 0.23.46 |
| `rustls-webpki` | 0.103.17 |
| `rustls-pki-types` | 1.16.0 |
| `sha2`, `hmac` | 0.11.1, 0.13.0 |
| `chacha20poly1305` | 0.11.0 |
| `chacha20`, `poly1305` | 0.10.2, 0.9.1 |
| `aes-gcm`, `aes` | 0.11.1, 0.9.4 |
| `x25519-dalek` | 3.0.0 |
| `curve25519-dalek` | 5.0.0 |
| `p256`, `ecdsa` | 0.14.0, 0.17.0, key process only |
| `libc` | 0.2.191 |
| `cmov`, `zeroize` | 0.5.4, 1.9.2 |
| `cpufeatures` | 0.3.1 |
| `ring` | 0.17.14, load client only |
