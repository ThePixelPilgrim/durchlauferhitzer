# qmail lessons — what durchlauferhitzer takes from qmail's security record

Date: 2026-11-09 · Owner decision: lessons 5 and 10 written into the spec
(NFR-RES-09, FR-PROC-08, NFR-SEC-14; open decisions 3 and 4). The rest are
carry-forwards in `HANDOFF.md`.

## Background

Pristine qmail 1.03 (1998, no third-party patches) has one known family of
security bugs in 28 years: Guninski's 2005 integer overflows
(CVE-2005-1513/1514/1515), 32-bit lengths in `stralloc` on 64-bit systems.
Bernstein declined the prize: his documented install runs `qmail-smtpd`
under `softlimit`, which makes the overflow unreachable. Debian's packaging
had no such limit, and in 2020 Qualys exploited it remotely for code
execution as `qmaild`, a user that owns nothing; their escalation went
through `qmail-verify`, a netqmail addition. The 2020 CVEs
(CVE-2020-3811/3812) are in that third-party code, not Bernstein's.

Why so few holes: a dozen small programs under seven uids with minimal
directories; every stage re-parses what the previous one hands it; no libc
string functions, every string carries its length and grows on demand;
about 15k lines in total; no features that need big parsers (no rewriting
rules, no `.forward` interpreter as root); dangerous defaults refused.

What did not hold: the threat model. Pristine qmail accepts a bare LF as a
line end, the shape the 2023 SMTP-smuggling research exploits; it accepts
first and bounces later, which made it a backscatter amplifier by 2004; it
has no TLS, AUTH, SPF or DKIM, so every real deployment runs third-party C
patches, and that is where the modern CVEs live.

## Lessons, mapped

Already requirements:

1. Length-carrying data, never fixed buffers — Rust's `usize` and slices;
   NFR-RES-01/03 fix every capacity at load; saturating counts.
2. Framing ambiguity is a hard rejection — FR-HTTP-03 (bare LF → 400),
   NFR-SEC-01/02.
3. Re-validate at every boundary — NFR-SEC-02/03: every forwarded byte
   against a model written from the spec.
4. Fuzz with a memory bound, not only for crashes — NFR-SEC-13.

Taken 2026-11-09:

5. Never let memory safety depend on the deployment — NFR-RES-09 (kernel
   memory backstop from `memory_limit`), FR-PROC-08 (`--check` warnings,
   the guaranteed install in the README, the example config shows the
   guaranteed values); open decision 4 (make `required` the default).
10. The threat model ages faster than the code — NFR-SEC-14, the
    framing-variant ledger, reviewed yearly and on every published
    variant. Its first pass surfaced the TE.0 row (a body on `GET`/`HEAD`
    forwarded verbatim): open decision 3.

Carry-forwards (not yet decided):

6. The platform moves under the code — qmail's `int` was right for every
   machine of 1998. Here: the per-arch seccomp allow-list and Landlock ABI
   assumptions; a new glibc using a new syscall under `seccomp_action
   kill` kills the server. The nightly tests the old side (Debian 12,
   musl); add a run on the newest kernel and glibc available.
7. The single C dependency is the lasting blind spot — qmail's resolver
   coupling carried its longest-lived correctness bug. Here: 42 distinct
   libc calls in the driver; `getpwnam_r` pulls NSS modules into a process
   that is still root. Accept a numeric `uid:gid` for `user` so a hardened
   install never enters NSS; document that the lookup runs as root.
8. Partition by process, not only by lint — qmail's containment held in
   2020 because the compromised component was its own process under its
   own uid. One process here; Rust and the confinement of `unsafe` to the
   `sys` module remove most of the reason. Revisit when TLS arrives: an
   opener process that owns the roots and passes descriptors over a unix
   socket is the cheap variant.
9. Code size is the lever — publish product line count beside the nightly
   benchmark rows and treat growth like a perf regression (a named
   reason). Watch the pull of nginx's other `X-Accel-*` features.
11. A refused feature becomes someone else's patch — name the supported
    deployment for TLS now (a terminator in front); when TLS comes, do it
    in-project in pure Rust rather than leave a vacuum.

Sources: Bernstein, "Some thoughts on security after ten years of qmail
1.0" (2007); Guninski's 2005 advisory; Qualys's 2020 advisory
(CVE-2005-1513, CVE-2020-3811/3812); Ubuntu USN-4556-1 and USN-4621-1.
