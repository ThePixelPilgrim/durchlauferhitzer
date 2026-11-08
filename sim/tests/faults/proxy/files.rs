//! The file headers of FR-PROXY-32 in the proxy net: routes drawn with
//! `internal_files` (both forms), forged file headers in client requests,
//! file headers in backend responses, and the spec's rules for each,
//! written independently of the core: no client's file header reaches a
//! backend, an `internal_files` route's backend gets Rack's hints, and no
//! backend's file header reaches a client. A backend here sends its route's
//! own header only on a non-`200`: the file a `200` names is not modelled
//! in this net (see `sim/tests/proxy/backend_files.rs`).

/// A route's `internal_files` (FR-CFG-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteFiles {
    XSendfile {
        root: &'static str,
    },
    XAccelRedirect {
        prefix: &'static str,
        root: &'static str,
    },
}

const ROOTS: &[&str] = &["/srv/files", "/srv/p"];
const PREFIXES: &[&str] = &["/protected/", "/"];

impl RouteFiles {
    /// None in two routes of four, else either form.
    pub(crate) fn random(rng: &mut fastrand::Rng) -> Option<RouteFiles> {
        let root = rng.choice(ROOTS).copied().unwrap_or("/srv/files");
        match rng.u8(0..4) {
            0 => Some(RouteFiles::XSendfile { root }),
            1 => Some(RouteFiles::XAccelRedirect {
                prefix: rng.choice(PREFIXES).copied().unwrap_or("/"),
                root,
            }),
            _ => None,
        }
    }

    /// The route setting, with its leading space.
    pub(crate) fn setting(files: Option<RouteFiles>) -> String {
        match files {
            None => String::new(),
            Some(RouteFiles::XSendfile { root }) => {
                format!(" internal_files x-sendfile {root};")
            }
            Some(RouteFiles::XAccelRedirect { prefix, root }) => {
                format!(" internal_files x-accel-redirect {prefix} {root};")
            }
        }
    }

    /// The response header the route trusts, lowercased.
    fn own(self) -> &'static str {
        match self {
            RouteFiles::XSendfile { .. } => "x-sendfile",
            RouteFiles::XAccelRedirect { .. } => "x-accel-redirect",
        }
    }
}

/// The request headers no client may send a backend (FR-PROXY-32
/// Stripping), lowercased.
const FORGEABLE: [&str; 4] = [
    "x-sendfile",
    "x-accel-redirect",
    "x-sendfile-type",
    "x-accel-mapping",
];

/// Whether a client's header (lowercased) is one of the four.
pub(crate) fn is_forged(name_lower: &str) -> bool {
    FORGEABLE.contains(&name_lower)
}

const FORGED: &[(&str, &[u8])] = &[
    ("X-Sendfile", b"/etc/passwd"),
    ("X-Sendfile", b"/srv/files/secret"),
    ("X-Accel-Redirect", b"/protected/../x"),
    ("X-Sendfile-Type", b"X-Sendfile"),
    ("X-Sendfile-Type", b"X-Accel-Redirect"),
    ("X-Accel-Mapping", b"/=/protected/"),
    ("X-Accel-Mapping", b"/srv/files/=/"),
];

/// Forged file header lines for a client request, one time in four: one
/// to three of them in any case, now and then also named in `Connection`.
pub(crate) fn forged_lines(rng: &mut fastrand::Rng) -> Vec<(String, Vec<u8>)> {
    let mut lines = Vec::new();
    if rng.u8(0..4) != 0 {
        return lines;
    }
    for _ in 0..rng.usize(1..=3) {
        if let Some(&(name, value)) = rng.choice(FORGED) {
            lines.push((any_case(rng, name), value.to_vec()));
        }
    }
    if rng.u8(0..4) == 0 {
        let named = rng.choice(FORGED).map_or("X-Sendfile", |(n, _)| n);
        lines.push(("Connection".to_owned(), any_case(rng, named).into_bytes()));
    }
    lines
}

fn any_case(rng: &mut fastrand::Rng, name: &str) -> String {
    match rng.u8(0..3) {
        0 => name.to_ascii_uppercase(),
        1 => name.to_ascii_lowercase(),
        _ => name.to_owned(),
    }
}

/// Rack's hints for the backend (FR-PROXY-32 Hints), right after
/// `x-forwarded-proto`.
pub(crate) fn hints(files: Option<RouteFiles>, out: &mut Vec<u8>) {
    match files {
        None => {}
        Some(RouteFiles::XSendfile { .. }) => {
            out.extend_from_slice(b"x-sendfile-type: X-Sendfile\r\n")
        }
        Some(RouteFiles::XAccelRedirect { prefix, root }) => out.extend_from_slice(
            format!("x-sendfile-type: X-Accel-Redirect\r\nx-accel-mapping: {root}/={prefix}\r\n")
                .as_bytes(),
        ),
    }
}

/// Whether a backend's header (lowercased) never reaches the client on a
/// route with `files` (FR-PROXY-32 Stripping): both file headers on every
/// route, every `x-accel-*` on an `x-accel-redirect` route.
pub(crate) fn is_stripped(name_lower: &str, files: Option<RouteFiles>) -> bool {
    match files {
        Some(RouteFiles::XAccelRedirect { .. }) => {
            name_lower.starts_with("x-accel-") || name_lower == "x-sendfile"
        }
        Some(RouteFiles::XSendfile { .. }) | None => {
            matches!(name_lower, "x-sendfile" | "x-accel-redirect")
        }
    }
}

/// File header lines for a backend response with status `code`, one time
/// in three, and the coverage they earn once relayed. The route's own
/// header only on a non-`200` (`code` is `None` when the status line is
/// not a parsed status). Other `x-accel-*` lines go with them.
pub(crate) fn response_lines(
    rng: &mut fastrand::Rng,
    files: Option<RouteFiles>,
    code: Option<u16>,
) -> (Vec<(&'static str, &'static [u8])>, Vec<&'static str>) {
    let mut lines = Vec::new();
    let mut tags = Vec::new();
    if rng.u8(0..3) != 0 {
        return (lines, tags);
    }
    let names: [&'static str; 2] = ["X-Sendfile", "X-Accel-Redirect"];
    for name in names {
        if rng.bool() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        let own = files.is_some_and(|f| f.own() == lower);
        match (own, code) {
            (true, Some(200) | None) => continue,
            (true, Some(_)) => tags.push("file-header-on-non-200"),
            (false, Some(200)) if files.is_some() => tags.push("other-file-header-on-200"),
            (false, _) => {}
        }
        let value: &'static [u8] = if rng.bool() {
            b"/srv/files/a"
        } else {
            b"/protected/a"
        };
        lines.push((name, value));
    }
    if rng.bool() {
        let extra: &[(&'static str, &'static [u8])] = &[
            ("X-Accel-Expires", b"10"),
            ("X-Accel-Buffering", b"no"),
            ("x-accel-limit-rate", b"1024"),
            ("X-ACCEL-CHARSET", b"utf-8"),
        ];
        if let Some(&line) = rng.choice(extra) {
            lines.push(line);
            if matches!(files, Some(RouteFiles::XAccelRedirect { .. })) {
                tags.push("x-accel-star-stripped");
            }
        }
    }
    (lines, tags)
}
