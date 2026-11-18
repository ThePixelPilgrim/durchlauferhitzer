# TLS helper interface — prior art and a draft

**This is a draft for the owner's decision. Nothing in it is decided or binding.**

2026-11-18. A research session surveyed existing protocols for handing a socket to a separate TLS program, and drafted an interface for the owner's direction of 2026-11-18. The same day's spike, [`docs/superpowers/spikes/2026-11-18-tls-helper.md`](../spikes/2026-11-18-tls-helper.md), tested the shape; where it contradicts or sharpens the draft, a note marked **Spike:** says so. Sources are marked as in the research: **VERIFIED** means read in that session, **FROM MEMORY** means a lead, not a fact.

**Verdict: no existing protocol can be adopted as is.** The two implementations of "hand back after the handshake" are library-internal blobs, and the one kernel protocol with our shape serves only in-kernel consumers. So we define a small interface of our own, from parts of four designs:

- **UCSPI-TLS and s6:** the exec convention, inherited descriptors, a `KEY=value` result and the exit-code style.
- **The Linux `handshake` family and `tlshd`:** the result "keys are installed on this socket, here is the status", a timeout set by the requester, and probing optional features before using them.
- **Cloudflare Keyless (`gokeyless`):** versioned framing, unknown tags ignored, and keys named by a hash for the key process.
- **RFC 9850, the TLS key log format:** the secret labels and hex encoding.

All algorithm identifiers come from the IANA registries.

## 1. The draft in one page

The server hands each accepted connection to a fresh helper process, the helper runs the handshake and exits, and the server carries the connection on. The interface has three properties:

| Property | Draft |
|---|---|
| Shape | One helper process per connection |
| Versioning | Per-connection lists; the helper chooses from them |
| Encoding | ASCII `key=value` lines, one datagram per message |

**Shape.** A small spawner, running as the helper's user, receives each request over a unix socket with `SCM_RIGHTS` and execs the helper. The exec picks up a replaced file at the next connection with no signal and no draining. A one-shot program is also the easiest thing for a distribution to build on its own TLS library.

**Versioning.** There is no global version negotiation. Each request states what the server can carry, and the helper may only pick from those lists. Unknown keys are ignored on both sides, and a `require=` line is the only must-understand mechanism.

**Encoding.** Each message is line-oriented ASCII `key=value`, in one `SOCK_SEQPACKET` datagram, ending with `end`. Every value comes from a fixed alphabet with a fixed maximum length. It is easy to debug with `strace` and matches the project's log lines.

What happens in the four version-skew situations:

- **Old server, new helper:** the request lacks the new keys, so the helper behaves as an old helper would. It uses only offered suites and modes and emits only wanted keys. Nothing breaks.
- **New server, old helper:** the old helper ignores new request keys. If the server marked a feature `require=`, the helper fails at once with `unsupported-required`, before reading the ClientHello. Otherwise the server sees the new result keys missing and uses its fallback.
- **Helper replaced at run time, now lacking something:** the next connection's result shows it, as a missing wanted key or `unsupported-required`. The result's `impl=` changes, so the server re-runs `--describe` and logs `action=capabilities_changed`. Connections in flight are unaffected.
- **Helper choosing something the server cannot carry:** this cannot happen by construction, because the helper chooses from the server's lists. A broken helper that returns something not offered commits a protocol violation, and the server drops that connection.

## 2. Record modes

The research recommends that the helper install the keys in the kernel by default. The controller's position, agreed with the owner on 2026-11-18, differs: the interface carries both modes, and the default is decided separately.

### The research's recommendation

The research makes `mode=ktls`, "keys installed in the kernel on this socket", the default. Exported secrets, `mode=secrets`, are a mode the server must offer. Its reasons:

- The result carries metadata only, so a leaked or logged result leaks nothing.
- The server needs no record-layer cryptography in the common case, and kTLS keeps `sendfile`.
- Third-party helpers have prior code: `tlshd` installs kTLS from GnuTLS state for all four kTLS ciphers (VERIFIED).
- A new cipher suite is the kernel's and the helper's business, so the server needs no change.

The research names two caveats that keep secrets in the picture:

1. **KeyUpdate.** When the client sends KeyUpdate, kTLS pauses decryption until someone provides the next receive key. The helper has exited, so either the server holds the traffic secrets and does one HKDF step, or it closes the connection.
2. **No kTLS, or a suite the kernel lacks.** The server then needs `mode=secrets` and its own record layer.

### The controller's position, agreed with the owner on 2026-11-18

The interface carries both modes. Which one is the default is a separate decision, taken after the measurements. The owner objected to kTLS as a requirement because of its CVE record; the kTLS spike counted 43 CVEs on its software path.

What each mode needs from the interface:

| Mode | The result must carry |
|---|---|
| Server record layer | Suite, key, IV and sequence number per direction |
| Same, honouring KeyUpdate | The traffic secret per direction, too |
| kTLS installed by the helper | Metadata only |
| Same, honouring KeyUpdate | The traffic secret per direction, too |

- **Spike:** the supported rustls export gives key, IV and sequence number, but not the traffic secret. The spike took the secret from rustls's `KeyLog` hook, a debugging interface.
- **Spike:** the draft's result carries only the traffic secrets, from which key and IV follow by HKDF. A helper on released rustls can fill them only through `KeyLog`.
- **Spike:** the kTLS spike found that any holder of a kTLS socket can read the keys back in clear. So a helper installing kTLS buys no secrecy over the server installing it from secrets.

## 3. Separate users

Three system users, one per role, are decided (owner, 2026-11-18). The research adds: no root process at run time, the helpers reached through a spawner that the init system starts as the helper's user, and the kernel vouching for each side's uid.

| User | Runs | Can read |
|---|---|---|
| `durchlauferhitzer` | the server | config, chains, content |
| `durchlauferhitzer-tls` | the spawner and the helpers | nothing on disk |
| `durchlauferhitzer-key` | the key process | the private keys |

- The server connects to the helper socket.
- The spawner, as the helper's user, connects to the key socket.
- The key process connects to nothing.
- Private keys are mode `0400`, owned by the key user.

### Who starts what

The research recommends a spawner started by the init system under the helper's user. It compares three options:

1. **A root spawner forked by the server,** switching uid per connection. It works with any init system. But it parses requests from the server, so it is one bug away from root. `CAP_SETUID` alone is root-equivalent for files.
2. **A spawner as its own service under the helper's user (recommended).** systemd `User=` and OpenRC `command_user` both start it. It needs no root and no uid switch at run time. It keeps one process per connection, as the spec's third guarantee needs.
3. **Socket activation with `Accept=yes`.** It needs no root, but it is systemd only. A unit instance per handshake is slow and rate-limited (`MaxConnections=` default 64, FROM MEMORY).

The spawner is ours, tiny and frozen; a distribution's helper installs only the helper executable. Restarting the spawner loses only handshakes in flight.

- **Spike:** option 2 worked. A spawner started as the helper's user cost the same as a root spawner: 1869 against 1872 µs of CPU per handshake.
- **Spike:** the uid switch of option 1 adds no measurable cost either.

All helpers and the spawner share a uid in option 2, so a compromised helper could ptrace a sibling or the spawner. The research's defences, in order:

- the spawner is non-dumpable (`PR_SET_DUMPABLE 0`);
- the seccomp filter installed before every exec denies `ptrace`, `process_vm_readv` and `process_vm_writev`, `pidfd_getfd`, and signals to other processes;
- Landlock forbids opening `/proc/<pid>/mem`;
- Yama `ptrace_scope` 1 or higher is a further layer, not relied on.

What the spike found:

- **Spike:** the helper must be non-dumpable too, not only the spawner. Without it, one helper attached to another with `strace`; with it, ptrace and `/proc/pid/mem` were refused.
- **Spike:** a seccomp filter before exec was not tried. A dynamic glibc helper needs `openat` at start-up; a static musl helper opens no file.

A uid per connection, as systemd's `DynamicUser=` does, would be stronger. It needs `CAP_SETUID` again, so the research leaves it as later hardening.

### How each side recognises the other

No secrets are involved: the kernel vouches for uids, and directory modes decide who may connect.

| Socket | Directory owner | Mode |
|---|---|---|
| `/run/durchlauferhitzer-tls/spawn.sock` | `durchlauferhitzer-tls:durchlauferhitzer` | `0750` |
| `/run/durchlauferhitzer-key/sign.sock` | `durchlauferhitzer-key:durchlauferhitzer-tls` | `0750` |

The checks:

- **Server to spawner:** after `connect`, the server checks with `SO_PEERCRED` that the listener is the helper's uid. The spawner checks the connecting peer is the server's uid.
- **Results:** they travel on a per-connection `SOCK_SEQPACKET` socketpair that the server creates. The server sets `SO_PASSCRED` and requires an `SCM_CREDENTIALS` uid equal to the helper's on every result.
- **Helper to key process:** the spawner connects once per connection and hands the descriptor to the child. The key process checks `SO_PEERCRED` and allows one signature per connection.

What the spike found:

- **Spike:** on a socketpair, `SO_PEERCRED` reports the pair's creator, not the process that writes. The research's detail already says so, and its summary line "`SO_PEERCRED`/`SCM_CREDENTIALS` checks" should be read that way: `SO_PEERCRED` only at `accept` or `connect`, `SCM_CREDENTIALS` on every message of a socketpair.
- **Spike:** that per-message check rejected a hand-over sent by uid 2003. It answers the research's open question 6.

What the packages create:

- the users, through `sysusers.d`, and on Debian `adduser --system --group` for systems without systemd;
- the runtime directories, through the units' `RuntimeDirectory=` or `tmpfiles.d`, and `checkpath` in the OpenRC scripts;
- the key directory `/etc/durchlauferhitzer/keys`, mode `0700`, owned by the key user;
- certificate chains readable by the server's user.

### How certificates reach a helper without file access

The server reads the chains, which are public, and writes a certificate bundle into a sealed `memfd`. The helper reads it with `pread` or `mmap`, so a large bundle costs nothing per connection.

- The seals are `F_SEAL_WRITE`, `F_SEAL_SHRINK`, `F_SEAL_GROW` and `F_SEAL_SEAL`.
- There is one bundle per configuration generation, passed with each request.
- Each key is named by the SHA-256 of its SubjectPublicKeyInfo, which is also how the key process finds it.
- The bundle lists the allowed signature schemes per certificate, so the helper need not parse X.509.

What the spike found:

- **Spike:** a memfd chain worked; reading it took 7 µs, and the helper closed it afterwards. The spike passed the raw chain, not the draft's bundle format.

### SELinux and AppArmor

Both are FROM MEMORY in the research; neither was tested. Without a policy, both leave the services unconfined, and everything works.

- **SELinux, Red Hat:** a confining module needs `fd use` between the domains, `tcp_socket` permissions, `connectto` on the unix sockets, and memfd permissions. A real module with a `-selinux` subpackage, tested on each RHEL major, is substantial work.
- **AppArmor, Debian and Ubuntu:** a profile needs network and unix rules with peer labels. Received files are revalidated against the receiving profile. It is less work than SELinux, but still work.

Plain Linux permissions put no limit on passing a socket between uids with `SCM_RIGHTS`.

- **Spike:** the client socket crossed from uid 2002 to uid 2001 on every handshake.
- **Spike:** the kTLS spike found that attaching the TLS ULP needs no privilege once the module is loaded. Loading it needs `CAP_NET_ADMIN`.

### Package layout

One source package builds several binary packages. A virtual package expresses the interface version, so any conforming helper satisfies the server.

- **`durchlauferhitzer`:** the server, the conformance tool, units and init scripts. `Depends: durchlauferhitzer-tls-interface-1, durchlauferhitzer-key`.
- **`durchlauferhitzer-tls`:** the native helper, the spawner and its unit, the helper's user. `Provides: durchlauferhitzer-tls-interface-1`, and it registers the helper with `update-alternatives`.
- **`durchlauferhitzer-key`:** the key process, its unit, the key user.
- **A distribution helper,** for example `durchlauferhitzer-tls-gnutls`: the helper only. `Provides: durchlauferhitzer-tls-interface-1`.

Notes on the layout:

- On Red Hat, the virtual name is `durchlauferhitzer-tls-interface(1)`.
- An incompatible interface, never planned, would get a new virtual name. A server could depend on `…-1 | …-2` during a transition.
- Open choice: the spawner in its own package, so a distribution helper does not pull in our native helper. The research thinks that likely right.

## 4. The concrete draft, interface version 1

The research wrote this section to be implementable alone. All integers are decimal ASCII unless the key says hex. Hex is lowercase on output and accepted in lowercase only.

### A. Invocation and descriptor layout

The spawner, or a test harness, execs the helper with this set-up:

- `argv`: `[path]`, or `[path, "--describe"]` for the capability list;
- environment: exactly `DLHTLS=1`; the spawner clears the rest;
- descriptors as below, all others closed.

| fd | What | Type |
|---|---|---|
| 0 | the client TCP socket, blocking | stream |
| 1 | the same socket | stream |
| 2 | diagnostics, never parsed | pipe |
| 3 | control channel to the server | seqpacket |
| 4 | certificate bundle, sealed | memfd |
| 5 | key channel, connected | seqpacket |

- fd 1 follows the UCSPI habit; a helper may use either.
- fd 2 takes free text lines, at most 64 KiB in total, logged by the server with rate limiting.
- fd 3 carries one request in and one result out.

The helper reads one datagram from fd 3, runs the handshake on fd 0, uses fd 5 for at most one signature, writes one datagram to fd 3 and exits. Exit status 0 follows `status=ok`, 1 follows `status=fail`, and anything else means the helper died. The server acts on the result, never on the status alone.

- **Spike:** the spike used another layout. The inbox socket was fd 3, the key channel fd 4 and the memfd fd 5; the client socket arrived later by `SCM_RIGHTS`. The draft's layout itself is untested.

### B. The capability description, `--describe`

The helper prints this on stdout and exits 0:

```
dlhtls-describe 1
interface=1
impl=native/0.4.0
modes=ktls,secrets
suites=1303,1301,1302
ktls_suites=1303,1301,1302
groups=001d,0017
sigschemes=0807,0403,0804
features=require,want,alpn,sni,secrets-with-ktls
end
```

| Key | Grammar, bound | Meaning |
|---|---|---|
| `interface` | `1` | the frozen core |
| `impl` | `[A-Za-z0-9._/+-]{1,64}` | implementation and version |
| `modes` | tokens, ≤ 8 | how it returns the connection |
| `suites` | 4-hex, ≤ 32 | suites it can negotiate |
| `ktls_suites` | 4-hex, ≤ 32 | suites it can install in kTLS |
| `groups` | 4-hex, ≤ 32 | informational |
| `sigschemes` | 4-hex, ≤ 32 | informational |
| `features` | tokens, ≤ 32 | names for `require=` and `want=` |

Unknown keys are ignored. The output is at most 4,096 bytes, and the server reads it with a 1 s timeout.

The server runs `--describe` at start, on reload, and when a result's `impl=` changes. It uses the output for three things, never for a security decision:

- to log what is installed;
- to warn at start when a required feature is missing;
- to decide which optional features to offer.

### C. The request, server to helper on fd 3

```
dlhtls-request 1
deadline_ms=10000
modes=ktls,secrets
suites=1303
ktls_suites=1303
alpn=6832,687474702f312e31
want=sni,alpn,secrets
require=
x-3fa91c07=9b2e
end
```

Every key, with when it is mandatory, its grammar and bound, and its meaning:

- **`deadline_ms`:** mandatory; 1 to 600000. The helper's own budget.
- **`modes`:** mandatory; a comma list of `ktls` and `secrets`, at most 8, in preference order. What the server can carry.
- **`suites`:** mandatory if `secrets` is offered; 4-hex list, at most 32. The suites the server's record layer implements.
- **`ktls_suites`:** mandatory if `ktls` is offered; 4-hex list, at most 32. The suites the running kernel supports; the server probes them at start.
- **`alpn`:** optional; a hex list, each entry 1 to 255 bytes, at most 16 entries, in server preference order. Absent means no ALPN.
- **`groups`:** optional; 4-hex list, at most 32. A limit on key-exchange groups.
- **`want`:** optional; tokens, at most 32. Result keys the server will parse beyond the core.
- **`require`:** optional; tokens, at most 32. Features the helper must honour or fail.

The `x-…` line is GREASE: the server adds one random unknown key to every request, so the ignore-unknown path stays exercised. Every helper should add one to its result, too.

The bundle on fd 4 uses the same line grammar, one block per certificate:

```
dlhtls-bundle 1
cert=0
key=<64 hex: SHA-256 of SPKI>
names=example.org,*.example.org
default=1
sigschemes=0403
chain=<hex DER cert>,<hex DER cert>
end
```

- `cert=` blocks repeat, at most 4,096 certificates.
- Each `chain` entry is at most 16 KiB of DER, and the whole bundle at most 64 MiB.
- The 1,024-byte line limit does not apply to `chain`, which has its own limit.
- Name matching tries an exact lowercase match first, then a one-label wildcard, then the `default=1` entry. No SNI also selects `default=1`.

### D. The result, helper to server on fd 3

Success in kTLS mode, with the secrets wanted:

```
dlhtls-result 1
status=ok
impl=native/0.4.0
version=0304
suite=1303
mode=ktls
cert=0
sni=example.org
alpn=6832
unread=0
client_secret=<64 hex>
server_secret=<64 hex>
client_seq=0
server_seq=0
end
```

Failure:

```
dlhtls-result 1
status=fail
impl=native/0.4.0
reason=no-common-suite
alert=40
end
```

Every key, with when it appears, its grammar and bound, and the server's check:

- **`status`:** always; `ok` or `fail`.
- **`impl`:** always; as in `--describe`. A change triggers a new `--describe`.
- **`reason`:** on fail; a token of at most 32 from the reason list. Logged.
- **`alert`:** on fail; `none` or 0 to 255. The alert already sent to the client; `none` means nothing was sent.
- **`version`:** on ok; must be `0304`.
- **`suite`:** on ok; 4-hex. Must be in `ktls_suites` for kTLS or in `suites` for secrets.
- **`mode`:** on ok; must be one of the offered modes.
- **`cert`:** on ok; 0 to 4095. Must exist in the bundle.
- **`sni`:** on ok, if wanted; `none` or a lowercase DNS name of at most 253, labels at most 63. Must match one of the certificate's names, or be `none` for the default.
- **`alpn`:** on ok, if wanted and offered; `none` or the hex of one offered ID.
- **`unread`:** on ok; must be `0` in version 1.
- **`client_secret`, `server_secret`:** on ok in `secrets` mode, or in `ktls` mode with `want=secrets`. Hex, 64 or 96 digits to match the suite's hash.
- **`client_seq`, `server_seq`:** with the secrets; 0 to 2^64−1. Both are 0 in version 1.

In `ktls` mode the server also checks that `getsockopt(TCP_ULP)` reads `tls` and that the installed cipher matches `suite`. It then waits for the helper's exit before its first `recvmsg`.

Reason tokens, append-only:

- `bad-request`, `unsupported-required`;
- `no-common-suite`, `no-common-group`, `no-alpn-match`, `no-certificate`;
- `client-alert`, `bad-client-message`, `deadline`;
- `signer-failed`, `ktls-failed`, `io-error`, `internal`.

What the spike found:

- **Spike:** the supported rustls export gives key, IV and sequence number, not `client_secret` and `server_secret`. The spike took them from the `KeyLog` hook.
- **Spike:** sequence numbers were 0 in both directions on every connection, as version 1 expects.
- **Spike:** the kTLS spike showed `getsockopt` returns the installed key, IV and live sequence number in clear. So the server's check of the installed cipher can work.

### E. Signing, helper to key process on fd 5

The same line grammar applies, one datagram each way, at most one exchange per channel.

```
dlhtls-sign 1
key=<64 hex: SHA-256 of SPKI>
scheme=0403
context=tls13-server-cv
transcript_hash=<64 or 96 hex>
end
```

```
dlhtls-signature 1
status=ok
signature=<hex, ≤ 1024 bytes>
end
```

On error, `status=fail` with `reason=` one of `unknown-key`, `scheme-not-allowed`, `bad-request`, `limit` or `internal`.

The key process does six things:

1. checks with `SO_PEERCRED` that the peer is the helper's uid;
2. accepts exactly one request per connection, then closes;
3. requires `context=tls13-server-cv`, the only context in version 1; an unknown context fails with `bad-request`, because a context is must-understand;
4. requires a hash of 32 or 48 bytes, and a scheme in the key's allowed list;
5. builds `0x20 × 64 || "TLS 1.3, server CertificateVerify" || 0x00 || hash` and signs it;
6. ignores unknown keys, receives with a credentials-only control buffer so any passed descriptor is dropped, and rate-limits per peer.

What the spike found:

- **Spike:** rustls asks the helper to sign the full 130-byte content, not a hash. So the helper strips the fixed prefix and sends the hash.
- **Spike:** the spike's key process took the full content and checked the prefix; that also refused a non-CertificateVerify input. One signature per connection was not enforced in the spike.
- **Spike:** either way, a compromised helper can get a signature over any transcript hash. Building the content in the key process and signing at most once per helper process are the limits the design can enforce.

### F. Rules for unknown and invalid items

1. **An unknown key in the request:** the helper ignores the line.
2. **An unknown value in a request list,** such as a suite it lacks: the helper ignores that value.
3. **`require=` names a feature the helper does not know or cannot honour:** the helper must not touch the socket. It answers `status=fail reason=unsupported-required alert=none`. `require=` is part of version 1, so every helper understands it.
4. **An unknown key in the result:** the server ignores it, unless it listed the key in `want=`. A security-relevant key the server did not ask for is ignored, too.
5. **A protocol violation:** the server closes the client connection without sending anything and logs `kind=tls_helper action=violation`. Violations are:
   - a known key with an invalid or out-of-range value;
   - a value outside what the request offered;
   - a duplicate key, or a missing mandatory key;
   - any bound exceeded.
6. **The header line** `dlhtls-request 1` or `dlhtls-result 1` numbers the frozen core and is expected to stay 1 forever. Any other number is a violation; a helper answers `reason=bad-request`.

The line grammar:

- a message is at most 16,384 bytes, one datagram; a truncated datagram (`MSG_TRUNC`) is a violation;
- a line is at most 1,024 bytes of printable ASCII `0x21` to `0x7E`, with no spaces, tabs or CR;
- keys are `[a-z0-9_-]{1,32}`, and each value has a per-key grammar;
- byte strings are lowercase hex, so no value needs escaping;
- unknown keys still pass the line checks, so they cannot carry bytes outside the alphabet.

The frozen core is the descriptor layout, the header lines, the line grammar, the ignore-unknown rule, `require=` and `want=`, `status`, `reason`, `alert` and `end`. Every algorithm is an IANA code point, so a new algorithm needs no interface change.

| IANA registry | Form |
|---|---|
| Cipher suites | 4 hex, e.g. `1303` |
| Supported groups | 4 hex, e.g. `001d` |
| Signature schemes | 4 hex, e.g. `0403` |
| ALPN IDs | hex of the bytes |
| Alert descriptions | decimal 0–255 |
| TLS versions | `0304` |

The project owns four append-only registries, never reused: `mode` values, `reason` tokens, feature names, and `--describe` keys.

### G. Who is trusted for what

The helper is untrusted by the server, and the key process trusts neither side beyond one well-formed signature per channel. The interface is shaped so that a lying helper can hurt only the connection it already controls.

What a lying helper can do, and the limit:

- **Wrong keys or suite:** that connection fails to decrypt. The server checks the suite was offered, and in kTLS mode compares the installed cipher.
- **Wrong ALPN:** the server speaks the wrong protocol to that client. ALPN must be one the server offered.
- **Wrong SNI:** the request goes to another virtual host the client could have asked for anyway. SNI must not be used for authorisation.
- **A socket in a bad state:** that connection breaks. The server keeps its own descriptor from `accept`, so the helper cannot substitute another socket.
- **Keeping a copy of the socket:** the server waits for the helper's exit before using the socket, and the process group is killed at the end.
- **A signing oracle:** it could impersonate the server in other handshakes while the compromise lasts. The limits are in section E.

A lying server can make the helper waste CPU or present a wrong certificate, nothing more. It already owns the socket and sees the plaintext.

- **Spike:** the spike did it the other way round. The server closed its copy and received the socket back from the helper by `SCM_RIGHTS`. Both work, but only the draft's way stops a helper from substituting a socket.
- **Spike:** the server cannot learn a helper's exit with `kill(pid, 0)`, because another uid gets `EPERM`. It must learn it from the spawner, or from EOF on the control channel.

### H. After the handshake

Version 1 forbids resumption, early data and post-handshake authentication, and sends no NewSessionTicket. KeyUpdate and close_notify are the server's.

- **A client KeyUpdate:** the server derives the next key from the secrets, or closes with close_notify if it has none. It answers with its own KeyUpdate when `update_requested` is set.
- **Tickets, if ever added:** ticket keys would live in the key process, not in per-connection helpers. The feature name `resumption` is reserved.
- **Client data arriving with the Finished:** the helper reads records exactly and stops after the client's Finished, so the first application record stays in the kernel queue. Version 1 fixes `unread=0`.

What the spike found:

- **Spike:** exact reading is required. A naive 16 KiB read lost the client's 54-byte request inside rustls, silently, and 0 of 252 connections succeeded.
- **Spike:** the kTLS spike observed that records queued before `TLS_RX` decrypt when the queue starts at a record boundary. A partly read record breaks the switch with `EMSGSIZE`.
- **Spike:** the server's record layer honoured client KeyUpdates of both kinds, with about 15 lines.

### I. Failure and abuse

The server owns every deadline, the client socket never leaves its descriptor table, and every channel is close-on-exec except the listed descriptors.

- **Deadlines:** `deadline_ms` is the helper's courtesy timer, 10 s by default. The server enforces a slightly longer hard deadline and has the spawner `SIGKILL` the process group, or kills it through a pidfd.
- **Limits set by the spawner before exec:** `RLIMIT_AS` about 64 MiB, `RLIMIT_NOFILE` about 16, `RLIMIT_NPROC` 0, `RLIMIT_CPU` about 2 s, `RLIMIT_CORE` 0, and `PR_SET_NO_NEW_PRIVS`.
- **Landlock** with no file-system access.
- **seccomp** denying `ptrace`, `process_vm_*`, `pidfd_getfd`, `pidfd_open`, signals outside the own process, `clone` without `CLONE_THREAD`, `clone3`, `execve`, `socket` and `connect`.
- **Helper death:** the server sees EOF without a result, closes the client socket without writing, and logs `action=died`.
- **Descriptor leaks:** the spawner closes everything above fd 5 with `close_range`. The server and key process receive with credentials-only control buffers, so a passed descriptor is dropped (`MSG_CTRUNC`, VERIFIED in `unix(7)`).
- **Floods:** the server caps the helpers in flight, and the spawner caps its children as a second line.

What the spike found:

- **Spike:** the server's uid cannot signal a helper; `kill` gave `EPERM`. A pidfd signal was not tried. So the spawner, which shares the helper's uid or is root, is the one that kills.
- **Spike:** the spike's helper ran under a 22-call allow-list, not the draft's deny-list, installed after start-up. Its own 10 s receive timeout and the spawner's 15 s kill were enough.
- **Spike:** in every failure case the server saw only EOF on the control channel, and logging one line was enough.

### J. Operations

The helper is a root-owned file at a fixed path, replaced by `rename`, chosen by `update-alternatives` when several are installed, and certified by a conformance tool shipped with the server.

- **Location:** `/usr/libexec/durchlauferhitzer/tls-helper`, owner `root:root`, mode `0755`. It is a symlink managed by `update-alternatives` on Debian and `alternatives` on Red Hat. None of the three service users can write it.
- **Atomic replace:** the new file is written in the same directory, synced, then renamed over the old name. dpkg and rpm already do this. Running helpers keep the old inode.
- **Logging:** at start and on change the server logs `kind=tls_helper action=described` with the `impl`, interface, modes and suites. A missing required feature is logged once per change.
- **Conformance:** `durchlauferhitzer-tls-conformance <helper-path>` runs without root. It drives the helper with its own TLS 1.3 client and fake key process, checks every rule above, and prints one `key=value` line per check.

What the spike found:

- **Spike:** `rename` gave 0 failed handshakes. An in-place `cp` failed handshakes, and with a pool it never succeeded.

## 5. Prior art

No candidate fits exactly. The fit column says partial or different problem; the research's own phrase follows in the notes.

| Candidate | Crosses | Fit | Mark |
|---|---|---|---|
| UCSPI-TLS, s6 | fds, env, plaintext pipes | partial | VERIFIED |
| BoringSSL split handshakes | opaque state blob | partial | VERIFIED |
| BoringSSL handshake hints | opaque hints | different | VERIFIED |
| s2n-tls serialize | opaque blob, keys | partial | VERIFIED |
| mbedTLS context save | opaque blob | different | VERIFIED |
| GnuTLS record state | keys via API | different | VERIFIED |
| rustls `KernelConnection` | secrets via API | different | VERIFIED |
| OpenSSL, wolfSSL | keys via API | different | FROM MEMORY |
| Cloudflare Keyless | sign requests, TLV | partial | VERIFIED |
| IETF LURK | key-schedule requests | partial | FROM MEMORY |
| PKCS#11, ssh-agent | generic sign | different | FROM MEMORY |
| relayd `ca` | sign requests, `imsg` | partial | VERIFIED |
| Linux `handshake`, `tlshd` | fd in, kTLS installed | partial | VERIFIED |
| PROXY v2 TLVs | session metadata | partial | VERIFIED |
| RFC 9850 key log | labelled secrets | partial | VERIFIED |
| qmail, Postfix, OpenSSH | privsep, signing | partial | FROM MEMORY |
| systemd fd store | sockets over restarts | different | VERIFIED |
| Design models | versioning rules | different | FROM MEMORY |

Notes on the table:

- "Partial" for BoringSSL split handshakes is the research's "same problem, cautionary tale".
- s2n-tls, mbedTLS and BoringSSL work only between the same library, often the same build.
- GnuTLS and rustls are building blocks a helper would use, not interfaces.
- The design models are CGI, FastCGI, LSP, 9P, virtio, Wayland, netlink, Protocol Buffers, TLS GREASE and HTTP's must-ignore rule.
- The research's rustls entry read `main`; `dangerous_extract_secrets` in 0.23 was FROM MEMORY. **Spike:** the spike confirmed it in 0.23.46.

### The Linux `handshake` family and `tlshd`

It is the closest existing "someone else does the handshake and installs the keys" protocol (VERIFIED). Only in-kernel consumers such as NFS and NVMe/TCP can start one; a userspace server cannot submit its own socket. Its shape is what we want: socket in, kTLS installed with `setsockopt`, a status out, and a timeout set by the requester. `tlshd` also probes the kernel's attribute policy before sending optional attributes, the same pattern as our `--describe`.

### UCSPI-TLS and s6-tlsd

It is the oldest convention for "the application does not contain TLS" (VERIFIED, from GitHub mirrors). The program is exec'ed with descriptors whose numbers are in environment variables, and metadata comes back as a `VAR=val` list. Its data path is the opposite of ours: the TLS process keeps the socket and tunnels plaintext. Its invocation style has needed no version change in about 20 years.

### BoringSSL split handshakes and handshake hints

BoringSSL had exactly our split and removed it as unmaintainable across versions. Its own header warned that "version skew may be fatal" and that the handback "contains sensitive key material". The replacement, handshake hints, is built so that version skew costs only performance: the local side applies hints "where applicable". The lessons: never ship library-internal state across a version boundary, and give the receiving side the final say.

The removal is sourced exactly so in the research: "**Removal (VERIFIED** via GitHub commit search on `repo:google/boringssl handoff`): commit `d3cdaa476a18`, 2026-10-06, 'Remove deprecated split handshakes support'. Current `main` `ssl.h` has no handoff functions (VERIFIED by grep)." The research also says `boringssl.googlesource.com` was refused and the GitHub mirror used, and that the commit search endpoint "worked once".

### Cloudflare Keyless and the OpenSSH monitor

Remote signing is a solved problem with stable wire protocols. Keyless has an 8-byte versioned header, a TLV body, and a parser that silently ignores unknown tags (VERIFIED). Its key server signs whatever digest it is sent, which our key process must not. The OpenSSH monitor, which signs for the unprivileged child, is FROM MEMORY; it has no version field and relies on both halves coming from one package.

## 6. Least sure about, and what was not reached

The research's least-sure list, with what the spikes have since found:

1. **Fork+exec cost per connection.** Spike: 1164 µs of CPU with static musl and 1872 µs with glibc, against 360 µs in-process. Landlock set-up was not measured.
2. **kTLS picking up already-queued bytes.** The kTLS spike observed it on 6.8 and 7.0, when the queue starts at a record boundary.
3. **`getsockopt(SOL_TLS)` returning enough to check the helper.** The kTLS spike observed the full key, IV and live sequence number.
4. **A seccomp filter for third-party TLS libraries.** Spike: a rustls helper ran under a 22-call allow-list. GnuTLS was not tried.
5. **KeyUpdate in kTLS mode:** how often real clients send it, and whether holding the secrets or closing is the better default. Still open.
6. **`SCM_CREDENTIALS` on socketpairs across the spawner.** Spike: it works and is the check to use.
7. **The SELinux module's scope on RHEL.** Still open.
8. **Whether one signature per key channel is always enough.** The research believes so, since HelloRetryRequest adds no second CertificateVerify. Not tested.

The proxy refused 15 hosts with HTTP 403, and one more did not resolve. GitHub mirrors stood in for most of them.

- `docs.rs`: rustls was read from GitHub source on `main` instead.
- `boringssl.googlesource.com`: the GitHub mirror was used.
- `datatracker.ietf.org`, `www.ietf.org`, `www.rfc-editor.org`: LURK was not read at all.
- `cr.yp.to`: qmail was not read.
- `www.kernel.org`, `man7.org`: `unix(7)` was read from a mirror.
- The others: `skarnet.org`, `web.archive.org`, `www.fehcom.de`, `www.haproxy.org`, `man.openbsd.org`, `p11-glue.github.io`, `www.freedesktop.org`.
- `www.suspectclass.com`, UCSPI-TLS's original home, does not resolve.
- `api.github.com` refused repository endpoints for repositories not attached to the session.

What the research could not verify:

- the LURK drafts;
- the user layouts of qmail and Postfix, OpenSSH privsep and its `sshd-session` split;
- Courier's use of UCSPI-TLS;
- OpenSSL, wolfSSL, p11-kit remote, the ssh-agent extension mechanism, Envoy, nginx, HAProxy, stunnel and hitch;
- SELinux and AppArmor behaviour for descriptor passing and `TCP_ULP`;
- the RFC 8446 CertificateVerify content and the `"traffic upd"` label, standard but not re-read; the spike used both successfully;
- the design models CGI, FastCGI, LSP, 9P, virtio, Wayland, Protocol Buffers, GREASE and HTTP.
