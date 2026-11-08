//! The model of backend-directed files in the proxy net (FR-PROXY-32,
//! FR-STATIC-19), written from the spec and not from the core: which
//! header value is a bad path (`502`), a `404` or a file; the status
//! (`200`, `206`, `304`, `416`) and the merged head; the exact file bytes
//! or the abort; the logs; and the upstream's fate, parked only after
//! `Content-Length: 0` ([`check_parked`]). What is drawn is
//! `internal.rs`.

use super::*;

/// Why a value that is no bad path names nothing to serve: a `404`
/// (FR-PROXY-32 Missing file, FR-STATIC-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotServed {
    /// No such entry, or an unservable one, or a file with a trailing
    /// `/`.
    Absent,
    /// A directory (the root alone among them).
    Dir,
    /// A decoded segment starting with `.`, or holding `/` or NUL.
    Dotfile,
}

/// What a backend's `200` with the route's file header names.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    /// A bad path: `502` (`BadFilePath`).
    Bad,
    /// The header given more than once: `502` too.
    Repeated,
    NotFound(NotServed),
    File(Rc<ModelFile>),
}

/// Percent-decodes a raw segment once; `None` for a malformed escape.
fn decode_once(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&b) = raw.get(i) {
        if b == b'%' {
            let hex = std::str::from_utf8(raw.get(i + 1..i + 3)?).ok()?;
            if !hex.bytes().all(|h| h.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
}

/// The spec's verdict on one value (FR-PROXY-32 Path, Bad path and the
/// planner and controller calls under it).
fn locate(files: RouteFiles, value: &[u8], tree: &FileTree) -> Target {
    if value.is_empty() || value.iter().any(|b| !(0x21..=0x7e).contains(b)) || value.contains(&b'?')
    {
        return Target::Bad;
    }
    let rest: &[u8] = match files {
        // The root alone, without its `/`: the root, a directory.
        RouteFiles::XSendfile { root } if value == root.as_bytes() => b"",
        RouteFiles::XSendfile { root } => {
            let Some(rest) = value
                .strip_prefix(root.as_bytes())
                .and_then(|r| r.strip_prefix(b"/"))
            else {
                return Target::Bad;
            };
            rest
        }
        RouteFiles::XAccelRedirect { prefix, .. } => {
            let Some(rest) = value.strip_prefix(prefix.as_bytes()) else {
                return Target::Bad;
            };
            rest
        }
    };
    let parts: Vec<&[u8]> = rest.split(|b| *b == b'/').collect();
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if (part.is_empty() && i != last) || *part == b"." || *part == b".." {
            return Target::Bad;
        }
    }
    let slash = parts[last].is_empty();
    let mut segments = Vec::new();
    for part in parts.iter().filter(|p| !p.is_empty()) {
        let decoded = match files {
            RouteFiles::XSendfile { .. } => part.to_vec(),
            RouteFiles::XAccelRedirect { .. } => match decode_once(part) {
                Some(d) => d,
                None => return Target::Bad,
            },
        };
        segments.push(decoded);
    }
    if segments
        .iter()
        .any(|s| s.starts_with(b".") || s.contains(&b'/') || s.contains(&0))
    {
        return Target::NotFound(NotServed::Dotfile);
    }
    // Walk the tree as a static route does (FR-STATIC-05…09), but a
    // directory is a 404.
    let mut key = String::new();
    let mut entry = &Entry::Dir;
    for s in &segments {
        match entry {
            Entry::Dir => {}
            Entry::File(_) | Entry::Unservable => return Target::NotFound(NotServed::Absent),
        }
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(&String::from_utf8_lossy(s));
        entry = match tree.nodes.get(&key) {
            Some(e) => e,
            None => return Target::NotFound(NotServed::Absent),
        };
    }
    match (entry, slash) {
        (Entry::File(f), false) => Target::File(Rc::clone(f)),
        (Entry::Dir, _) => Target::NotFound(NotServed::Dir),
        (Entry::File(_), true) | (Entry::Unservable, _) => Target::NotFound(NotServed::Absent),
    }
}

/// A backend's `200` naming a file, and the model of what the client
/// gets for it (completed by the request in [`file_alts`]).
#[derive(Debug, Clone)]
pub(crate) struct FileReply {
    pub(crate) target: Target,
    pub(crate) policy: Policy,
    /// The backend's lines the merged head keeps, as relayed.
    pub(crate) kept: Vec<(String, Vec<u8>)>,
    pub(crate) backend_date: bool,
    pub(crate) backend_type: bool,
    pub(crate) backend_nosniff: bool,
    /// The head says `Content-Length: 0` (FR-PROXY-02's pooling rule).
    pub(crate) zero: bool,
}

impl FileReply {
    /// The backend's head: `lines` (lowercased names, trimmed values) of
    /// a `200` on a route with `files`, its `Connection` `tokens`.
    pub(crate) fn new(
        trigger: &Trigger,
        lines: &[(String, Vec<u8>)],
        tokens: &[String],
        ctx: &Ctx,
        zero: bool,
    ) -> Option<FileReply> {
        let (files, tree) = (ctx.files?, ctx.tree.as_deref()?);
        let target = match trigger.values.as_slice() {
            [one] => locate(files, trim_ows(one), tree),
            _ => Target::Repeated,
        };
        let mut reply = FileReply {
            target,
            policy: ctx.policy,
            kept: Vec::new(),
            backend_date: false,
            backend_type: false,
            backend_nosniff: false,
            zero,
        };
        const REPLACED: [&str; 6] = [
            "content-length",
            "transfer-encoding",
            "etag",
            "last-modified",
            "accept-ranges",
            "content-range",
        ];
        for (name, value) in lines {
            if is_hop(name, tokens) || is_stripped(name, Some(files)) || REPLACED.contains(&&**name)
            {
                continue;
            }
            reply.backend_date |= name == "date";
            reply.backend_type |= name == "content-type";
            reply.backend_nosniff |= name == "x-content-type-options";
            reply.kept.push((name.clone(), value.clone()));
        }
        Some(reply)
    }

    pub(crate) fn size(&self) -> u64 {
        match &self.target {
            Target::File(f) => f.size(),
            Target::Bad | Target::Repeated | Target::NotFound(_) => 0,
        }
    }

    /// Served (not a bad path): the upstream may be parked after it.
    fn served(&self) -> bool {
        match self.target {
            Target::Bad | Target::Repeated => false,
            Target::NotFound(_) | Target::File(_) => true,
        }
    }
}

/// The tokens of every `Connection` line of a backend head (lowercased
/// names, trimmed values), lowercased.
pub(crate) fn tokens_of(lines: &[(String, Vec<u8>)]) -> Vec<String> {
    lines
        .iter()
        .filter(|(n, _)| n == "connection")
        .flat_map(|(_, v)| {
            String::from_utf8_lossy(v)
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The merged head of a `200`, `206` or `304` (planner calls under
/// FR-PROXY-32): the core's status line, its `Date` unless the backend
/// sent one, the backend's kept lines, then the core's lines in the
/// order of FR-STATIC-19.
fn merged_head(
    f: &FileReply,
    file: &ModelFile,
    status: &str,
    part: Option<(u64, u64)>,
    persistence: Persistence,
) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    if !f.backend_date {
        out.push_str(&format!("Date: {}\r\n", NET_DATE.1));
    }
    let mut head = out.into_bytes();
    for (name, value) in &f.kept {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
    let validators = format!(
        "ETag: {}\r\nLast-Modified: {}\r\n",
        model_etag(file.stamp),
        model_imf_fixdate(file.stamp.modified_ns / 1_000_000_000)
    );
    let mut lines = String::new();
    if status.starts_with("304") {
        lines.push_str(&validators);
    } else {
        if !f.backend_type {
            lines.push_str(&format!("Content-Type: {}\r\n", file.content_type));
        }
        let size = file.size();
        match part {
            None => lines.push_str(&format!("Content-Length: {size}\r\n")),
            Some((first, last)) => lines.push_str(&format!(
                "Content-Length: {}\r\nContent-Range: bytes {first}-{last}/{size}\r\n",
                last - first + 1
            )),
        }
        lines.push_str(&validators);
        lines.push_str("Accept-Ranges: bytes\r\n");
        if !f.backend_nosniff {
            lines.push_str("X-Content-Type-Options: nosniff\r\n");
        }
    }
    if persistence == Persistence::Close {
        lines.push_str("Connection: close\r\n");
    }
    lines.push_str("\r\n");
    head.extend_from_slice(lines.as_bytes());
    head
}

/// The outcomes of a backend-directed file for `req`, with each client
/// persistence in `bases` (the relay's, FR-PROXY-27, -28): `cont` before
/// the head, `up` what the backend gets.
#[allow(clippy::too_many_arguments, reason = "the parts of one exchange")]
pub(crate) fn file_alts(
    req: &Request,
    f: &FileReply,
    bases: &[Persistence],
    cont: &[u8],
    up: &BackendExp,
    who: &Who,
    backend: BackendId,
    tags: &[&'static str],
) -> Vec<Alt> {
    let head = &req.wire[..req.head_len];
    let method = &req.summary.method;
    let mode = if req.head_only {
        BodyMode::HeadOnly
    } else {
        BodyMode::Full
    };
    let response = |status| Log::Response {
        conn: who.conn,
        peer: who.peer,
        status,
        request: Some(req.summary.clone()),
    };
    let served = |status| Log::BackendFile {
        conn: who.conn,
        peer: who.peer,
        backend,
        status,
        request: req.summary.clone(),
    };
    let mut out = Vec::new();
    for &p in bases {
        let mut t = tags.to_vec();
        let mut bytes = cont.to_vec();
        let mut logs = Vec::new();
        let mut cut = None;
        match &f.target {
            Target::Bad | Target::Repeated => {
                t.push(match f.target {
                    Target::Repeated => "file-502-repeated",
                    Target::Bad | Target::NotFound(_) | Target::File(_) => "file-502-bad-path",
                });
                bytes.extend(fixed(Status::BadGateway, p, mode));
                logs.push(Log::BackendFailed {
                    conn: who.conn,
                    peer: who.peer,
                    backend,
                    kind: BackendFailure::BadFilePath,
                });
                logs.push(response(Status::BadGateway));
            }
            Target::NotFound(why) => {
                t.push(match why {
                    NotServed::Absent => "file-404-missing",
                    NotServed::Dir => "file-404-dir",
                    NotServed::Dotfile => "file-404-dotfile",
                });
                bytes.extend(fixed(Status::NotFound, p, mode));
                logs.push(response(Status::NotFound));
            }
            Target::File(file) => {
                // Conditionals for a GET or HEAD only, a range for a GET
                // only (FR-STATIC-18, -19; the calls under FR-PROXY-32).
                let conditional = matches!(method, Method::Get | Method::Head)
                    && model_verdict(head, file.stamp, NET_DATE.0) == ModelFreshness::NotModified;
                let ranged = match method {
                    Method::Get => model_ranged(head, file.stamp),
                    Method::Head | Method::Other(_) => ModelRanged::Whole,
                };
                match (conditional, ranged) {
                    (true, _) => {
                        t.push("file-304");
                        bytes.extend(merged_head(f, file, "304 Not Modified", None, p));
                        logs.push(served(Status::NotModified));
                    }
                    (false, ModelRanged::Unsatisfiable) => {
                        t.push("file-416");
                        bytes.extend(model_range_not_satisfiable(
                            file.size(),
                            p,
                            Some(NET_DATE.1),
                        ));
                        logs.push(response(Status::RangeNotSatisfiable));
                    }
                    (false, ModelRanged::Whole | ModelRanged::Part(..)) => {
                        let (status, part, span) = match ranged {
                            ModelRanged::Part(a, b) => (
                                "206 Partial Content",
                                Some((a, b)),
                                Span {
                                    first: a,
                                    end: b + 1,
                                },
                            ),
                            ModelRanged::Whole | ModelRanged::Unsatisfiable => {
                                ("200 OK", None, Span::whole(file.size()))
                            }
                        };
                        t.push(match part {
                            Some(_) => "file-206",
                            None => "file-200",
                        });
                        t.push(match f.backend_type {
                            true => "file-backend-content-type",
                            false => "file-name-content-type",
                        });
                        match method {
                            Method::Head => t.push("file-head"),
                            Method::Other(m) if m == "POST" => t.push("file-post"),
                            Method::Get | Method::Other(_) => {}
                        }
                        bytes.extend(merged_head(f, file, status, part, p));
                        logs.push(served(match part {
                            Some(_) => Status::PartialContent,
                            None => Status::Ok,
                        }));
                        if !req.head_only {
                            let range = usize::try_from(span.first).unwrap_or(usize::MAX)
                                ..usize::try_from(span.end).unwrap_or(usize::MAX);
                            bytes.extend_from_slice(file.bytes.get(range).unwrap_or_default());
                            cut = file.expected_abort(f.policy, span).map(|(e, _)| e);
                        }
                    }
                }
            }
        }
        if !f.zero && f.served() {
            t.push("file-closed-body");
        }
        if let Some(error) = cut {
            t.push("file-changed-abort");
            logs.push(Log::FileAborted {
                conn: who.conn,
                peer: who.peer,
                error,
            });
        }
        out.push(Alt {
            bytes,
            cut: cut.is_some(),
            logs,
            closes: p == Persistence::Close || cut.is_some(),
            upstream: Some(up.clone()),
            tags: t,
        });
    }
    out
}

/// Whether `ex` (run as `run`) may leave its upstream parked after a
/// file (FR-PROXY-32, FR-PROXY-02): a served name, `Content-Length: 0`,
/// not a HEAD, the exchange reusable as for a relayed response (a pooled
/// route, an HTTP/1.1 client, the whole request written, an HTTP/1.1
/// backend without `close`) and nothing behind the head. `None` when the
/// exchange is no file's. With [`Fate::Reused`] the route and what the
/// backend received are left to the reuse check (`check.rs`): the last
/// bytes of the request may not have been collected yet.
pub(crate) fn file_parkable(ex: &Exchange, run: &Run, seen: Fate) -> Option<Result<(), String>> {
    let script = ex.script.as_ref()?;
    let Reply::File(f) = &script.reply else {
        return None;
    };
    let Verdict::Forward(fwd) = &ex.req.verdict else {
        return Some(Err("its request was not forwarded".to_owned()));
    };
    let why = if !f.served() {
        "its backend named a bad path"
    } else if !f.zero {
        "its backend's head had no `Content-Length: 0`"
    } else if ex.req.head_only {
        "it answered a HEAD"
    } else if matches!(seen, Fate::Parked(net) if !net.routes[fwd.route].pool) {
        "its route says `pool off`"
    } else if ex.req.http10 {
        "its client spoke HTTP/1.0"
    } else if matches!(seen, Fate::Parked(_)) && run.got != fwd.bytes {
        "its request was not written whole"
    } else if !script.keeps {
        "its backend said close or spoke HTTP/1.0"
    } else if sent_len(script) > script.head_end {
        "bytes came behind its head"
    } else {
        return Some(Ok(()));
    };
    Some(Err(why.to_owned()))
}

/// When [`file_parkable`] judges an upstream.
#[derive(Clone, Copy)]
pub(crate) enum Fate<'a> {
    /// Idle in the pool, or reused by a later exchange that is judged
    /// with the route and every received byte (`left_reusable`).
    Parked(&'a NetConfig),
    /// Just handed to a later exchange (`pool.rs`), before what the
    /// backend received is all collected.
    Reused,
}

/// How many of `script`'s bytes its backend sends before its ending.
fn sent_len(script: &Script) -> usize {
    match script.ending {
        Ending::Done { .. } => script.wire.len(),
        Ending::Reset(k) | Ending::ResetAfter(k) | Ending::Eof(k) => k,
    }
}

/// Every upstream the core holds idle now: if a file's exchange left it
/// there, the model must allow it (FR-PROXY-32: "pooled only after
/// `Content-Length: 0`"); seen once, the run says so (`file-parked`).
pub(crate) fn check_parked(sim: &Sim, stories: &mut [Story], assign: &Assign, net: &NetConfig) {
    for up in sim.upstreams() {
        if sim.upstream_holder(up) != Some(UpstreamHolder::Pool) {
            continue;
        }
        let Some((conn, i, _)) = assign.last_of(up) else {
            continue;
        };
        let Some(story) = stories.iter_mut().find(|s| s.conn == conn) else {
            continue;
        };
        let (ex, run) = (&story.exchanges[i], &story.runs[i]);
        match file_parkable(ex, run, Fate::Parked(net)) {
            None => {}
            Some(Ok(())) => story.runs[i].parked = true,
            Some(Err(why)) => panic!(
                "conn={} exchange {i}: up={} parked after a file, but {why}\n\
                 request {:?}\nscript {:?}\ntrace:\n{}",
                conn.0,
                up.0,
                String::from_utf8_lossy(&ex.req.wire),
                ex.script,
                sim.trace()
            ),
        }
    }
}

/// The file coverage names, each required at least [`MIN_FILE_HITS`]
/// times over the seeds.
pub(crate) const FILE_REQUIRED: &[&str] = &[
    "file-200",
    "file-206",
    "file-304",
    "file-416",
    "file-404-missing",
    "file-404-dir",
    "file-404-dotfile",
    "file-502-bad-path",
    "file-502-repeated",
    "file-head",
    "file-post",
    "file-parked",
    "file-closed-body",
    "file-changed-abort",
    "file-backend-content-type",
    "file-name-content-type",
];

/// Each file name must be reached well clear of zero (a 1000-seed run).
pub(crate) const MIN_FILE_HITS: usize = 15;

/// The file coverage of `seen`, scaled to the seeds run: at least
/// [`MIN_FILE_HITS`] per 1000 seeds (and at least one in a short range).
pub(crate) fn check_file_coverage(seen: &BTreeMap<&'static str, usize>, seeds: u64) {
    let want = (MIN_FILE_HITS as u64 * seeds / PROXY_SEEDS).max(1) as usize;
    for name in FILE_REQUIRED {
        let n = seen.get(name).copied().unwrap_or(0);
        assert!(
            n >= want,
            "{name} reached {n} times in the proxy seeds (want {want})"
        );
    }
}

/// The model's bad-path rules, from the spec, on fixed values.
#[test]
fn the_model_judges_paths_from_the_spec() {
    let mut tree = FileTree {
        root: "/srv/files",
        nodes: BTreeMap::new(),
        unservable: Vec::new(),
    };
    let file = Rc::new(ModelFile {
        bytes: b"x".to_vec(),
        stamp: FileStamp {
            size: 1,
            modified_ns: 1,
        },
        content_type: "text/plain; charset=utf-8",
        fault: Fault::None,
    });
    tree.nodes.insert("d".to_owned(), Entry::Dir);
    tree.nodes
        .insert("d/a.txt".to_owned(), Entry::File(Rc::clone(&file)));
    tree.nodes.insert("s".to_owned(), Entry::Unservable);
    let sf = RouteFiles::XSendfile { root: "/srv/files" };
    let ar = RouteFiles::XAccelRedirect {
        prefix: "/protected/",
        root: "/srv/files",
    };
    let kind = |files, value: &str| match locate(files, value.as_bytes(), &tree) {
        Target::Bad => "bad",
        Target::Repeated => "repeated",
        Target::NotFound(NotServed::Absent) => "missing",
        Target::NotFound(NotServed::Dir) => "dir",
        Target::NotFound(NotServed::Dotfile) => "dotfile",
        Target::File(_) => "file",
    };
    for (files, value, want) in [
        (sf, "/srv/files/d/a.txt", "file"),
        (sf, "/srv/files", "dir"),
        (sf, "/srv/files/", "dir"),
        (sf, "/srv/files/d/a.txt/", "missing"),
        (sf, "/srv/files/s", "missing"),
        (sf, "/srv/files/.env", "dotfile"),
        (sf, "/srv/files/d/%61.txt", "missing"),
        (sf, "/srv/filesx/d/a.txt", "bad"),
        (sf, "/srv/files/d/../a.txt", "bad"),
        (sf, "/srv/files//d", "bad"),
        (sf, "/srv/files/d/a.txt?x", "bad"),
        (sf, "/srv/files/d/a .txt", "bad"),
        (sf, "", "bad"),
        (ar, "/protected/d/%61.txt", "file"),
        (ar, "/protected/", "dir"),
        (ar, "/protected", "bad"),
        (ar, "/protected/%2e%2e/x", "dotfile"),
        (ar, "/protected/..%2fx", "dotfile"),
        (ar, "/protected/a%00b", "dotfile"),
        (ar, "/protected/..", "bad"),
        (ar, "/protected/d/a.txt%zz", "bad"),
        (ar, "/protected/d/a.txt%4", "bad"),
    ] {
        assert_eq!(kind(files, value), want, "{value}");
    }
}
