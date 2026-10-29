//! The log format's property (FR-LOG-05, NFR-SEC-07; Review Focus 3): a
//! seeded test that no entry, whatever its untrusted strings, writes more
//! than one line or a raw control byte, that every entry fits the sink's
//! 64 KiB buffer, and that each untrusted value, cut at 4 KiB or not
//! (OA4; the exact cut lines are unit tests in `src/log/format.rs`),
//! parses back. The exact lines are in `log.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};

use durchlauferhitzer::log::format_entry;
use durchlauferhitzer_core::http::request::{HttpError, Method};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{BackendId, ConnId, Log, RejectReason, RequestLine, RequestSummary};

/// The backends the core entries name by index (`Core::backends`).
fn backends() -> Vec<SocketAddr> {
    vec![
        "127.0.0.1:9000".parse().unwrap(),
        "[::1]:9000".parse().unwrap(),
        SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 8080, 0, 2)),
    ]
}

/// A seeded xorshift64* generator: deterministic, so a failure names the
/// seed that reproduces it, and no dev-dependency is needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A string mixing plain bytes, the escaped ASCII bytes and arbitrary
    /// code points (all of them non-ASCII bytes once encoded); one in
    /// eight is long, about as often over the 4 KiB cap as not (OA4).
    fn string(&mut self) -> String {
        let len = if self.below(8) == 0 {
            500 + self.below(1000)
        } else {
            self.below(24)
        };
        (0..len)
            .map(|_| match self.below(4) {
                0 => char::from(b"aZ09._:/@%+-"[self.below(12) as usize]),
                1 => char::from(b"\r\n\0\t\x1b\x7f\"\\= ?#"[self.below(12) as usize]),
                2 => char::from_u32(self.below(0x11_0000) as u32).unwrap_or('\u{fffd}'),
                _ => char::from(self.below(128) as u8),
            })
            .collect()
    }

    fn method(&mut self) -> Method {
        match self.below(3) {
            0 => Method::Get,
            1 => Method::Head,
            _ => Method::Other(self.string()),
        }
    }

    fn entry(&mut self) -> Log {
        let conn = ConnId(self.next());
        let peer = IpAddr::V4(Ipv4Addr::from(self.next() as u32));
        let request = RequestSummary {
            method: self.method(),
            host: (self.below(2) == 0).then(|| self.string()),
            path: self.string(),
        };
        match self.below(3) {
            0 => Log::Response {
                conn,
                peer,
                status: Status::Ok,
                request: Some(request),
            },
            1 => Log::ProxiedResponse {
                conn,
                peer,
                // One past the list too: the index fallback.
                backend: BackendId(self.below(4) as u32),
                status: self.below(600) as u16,
                request,
            },
            _ => Log::Rejected {
                conn,
                peer,
                reason: RejectReason::Http {
                    error: HttpError::BadHeaderValue,
                    request: Some(RequestLine {
                        method: request.method,
                        path: request.path,
                    }),
                },
            },
        }
    }
}

/// The grammar's plain bytes (FR-LOG-05).
fn is_plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._:/@%+-[]".contains(&byte)
}

/// How long `raw` is as written, plain or `quoted` (FR-LOG-05).
fn written_len(raw: &[u8], quoted: bool) -> usize {
    if !quoted {
        return raw.len();
    }
    2 + raw
        .iter()
        .map(|&b| match b {
            b'"' | b'\\' => 2,
            0x21..=0x7e if b != b'=' => 1,
            _ => 4,
        })
        .sum::<usize>()
}

/// Whether `raw` is written quoted.
fn is_quoted(raw: &[u8]) -> bool {
    raw.is_empty() || !raw.iter().all(|&b| is_plain(b))
}

/// The cap on an untrusted value as written (OA4).
const MAX_VALUE: usize = 4096;

/// Splits a formatted line into its values, undoing the escaping; `None`
/// if the line breaks the grammar.
fn parse(line: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let body = line.strip_suffix(b"\n")?;
    let mut fields = Vec::new();
    let mut rest = body;
    loop {
        let eq = rest.iter().position(|&b| b == b'=')?;
        let key = std::str::from_utf8(&rest[..eq]).ok()?.to_owned();
        rest = &rest[eq + 1..];
        let mut value = Vec::new();
        if rest.first() == Some(&b'"') {
            let mut i = 1;
            loop {
                match *rest.get(i)? {
                    b'"' => break,
                    b'\\' => match *rest.get(i + 1)? {
                        b'"' => {
                            value.push(b'"');
                            i += 2;
                        }
                        b'\\' => {
                            value.push(b'\\');
                            i += 2;
                        }
                        b'x' => {
                            let hex = std::str::from_utf8(rest.get(i + 2..i + 4)?).ok()?;
                            value.push(u8::from_str_radix(hex, 16).ok()?);
                            i += 4;
                        }
                        _ => return None,
                    },
                    b => {
                        if !(0x21..0x7f).contains(&b) || b == b'=' {
                            return None;
                        }
                        value.push(b);
                        i += 1;
                    }
                }
            }
            rest = &rest[i + 1..];
        } else {
            let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
            value.extend_from_slice(&rest[..end]);
            if value.is_empty() || !value.iter().all(|&b| is_plain(b)) {
                return None;
            }
            rest = &rest[end..];
        }
        fields.push((key, value));
        match rest.first() {
            None => return Some(fields),
            Some(b' ') => rest = &rest[1..],
            Some(_) => return None,
        }
    }
}

#[test]
fn any_entry_is_one_line_without_raw_control_bytes() {
    // Values cut, and long values (over 1 KiB raw) left whole.
    let (mut cut, mut long_whole) = (0, 0);
    for seed in 1..=20_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let entry = rng.entry();
        let mut out = Vec::new();
        format_entry(&entry, &backends(), &mut out);
        assert_eq!(
            out.iter().filter(|&&b| b == b'\n').count(),
            1,
            "seed {seed}: {entry:?}"
        );
        assert_eq!(out.last(), Some(&b'\n'), "seed {seed}");
        // OA4: every entry fits the sink's 64 KiB buffer.
        assert!(out.len() < 64 * 1024, "seed {seed}: {}", out.len());
        assert!(
            out[..out.len() - 1]
                .iter()
                .all(|&b| (0x20..0x7f).contains(&b)),
            "seed {seed}: {:?}",
            String::from_utf8_lossy(&out)
        );
        // The line parses back to the untrusted strings, byte for byte.
        let fields = parse(&out)
            .unwrap_or_else(|| panic!("seed {seed}: {:?}", String::from_utf8_lossy(&out)));
        // The keys of the kind, in the fixed order (FR-LOG-05).
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        let host_key = |r: &RequestSummary| r.host.as_ref().map(|_| "host");
        let expected: Vec<&str> = match &entry {
            Log::Response {
                request: Some(r), ..
            } => ["kind", "conn", "peer", "status", "method"]
                .into_iter()
                .chain(host_key(r))
                .chain(["path"])
                .collect(),
            Log::ProxiedResponse { request: r, .. } => {
                ["kind", "conn", "peer", "backend", "status", "method"]
                    .into_iter()
                    .chain(host_key(r))
                    .chain(["path"])
                    .collect()
            }
            Log::Rejected { .. } => vec!["kind", "conn", "peer", "reason", "method", "path"],
            other @ (Log::Response { request: None, .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::BackendFailed { .. }) => panic!("not generated: {other:?}"),
        };
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let (method, host, path) = match &entry {
            Log::Response {
                request: Some(r), ..
            }
            | Log::ProxiedResponse { request: r, .. } => (&r.method, r.host.clone(), &r.path),
            Log::Rejected {
                reason:
                    RejectReason::Http {
                        request: Some(r), ..
                    },
                ..
            } => (&r.method, None, &r.path),
            other @ (Log::Response { .. }
            | Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::BackendFailed { .. }) => panic!("not generated: {other:?}"),
        };
        let method = match method {
            Method::Get => "GET".to_owned(),
            Method::Head => "HEAD".to_owned(),
            Method::Other(m) => m.clone(),
        };
        // OA4: a value too long as written is followed by its
        // `<key>_truncated`, right after it.
        let raws = [
            ("method", Some(method)),
            ("host", host),
            ("path", Some(path.clone())),
        ];
        let too_long = |key: &str| {
            raws.iter().any(|(k, raw)| {
                *k == key
                    && raw.as_ref().is_some_and(|raw| {
                        written_len(raw.as_bytes(), is_quoted(raw.as_bytes())) > MAX_VALUE
                    })
            })
        };
        let expected: Vec<String> = expected
            .into_iter()
            .flat_map(|key| {
                let cut = too_long(key).then(|| format!("{key}_truncated"));
                std::iter::once(key.to_owned()).chain(cut)
            })
            .collect();
        assert_eq!(keys, expected, "seed {seed}");
        for (key, raw) in raws {
            let Some(raw) = raw else {
                assert_eq!(get(key), None, "seed {seed}");
                continue;
            };
            let raw = raw.into_bytes();
            let value = get(key).unwrap();
            let truncated = get(&format!("{key}_truncated"));
            let Some(truncated) = truncated else {
                assert_eq!(value, raw, "seed {seed}: {key}");
                if raw.len() > 1024 {
                    long_whole += 1;
                }
                continue;
            };
            cut += 1;
            // The shown part is the longest prefix that fits, and the
            // count is the rest of the raw bytes.
            let quoted = is_quoted(&raw);
            let left: usize = String::from_utf8(truncated).unwrap().parse().unwrap();
            assert!(raw.starts_with(&value), "seed {seed}: {key}");
            assert_eq!(value.len() + left, raw.len(), "seed {seed}: {key}");
            assert!(left > 0, "seed {seed}: {key}");
            assert!(
                written_len(&value, quoted) <= MAX_VALUE,
                "seed {seed}: {key}"
            );
            assert!(
                written_len(&raw[..=value.len()], quoted) > MAX_VALUE,
                "seed {seed}: {key} cut short"
            );
        }
        if let Log::ProxiedResponse { backend, .. } = &entry {
            let expected = usize::try_from(backend.0)
                .ok()
                .and_then(|index| backends().get(index).copied())
                .map_or(backend.0.to_string(), |address| address.to_string());
            assert_eq!(get("backend"), Some(expected.into_bytes()), "seed {seed}");
        }
    }
    assert!(
        cut > 500 && long_whole > 500,
        "cut {cut}, long and whole {long_whole}"
    );
}
