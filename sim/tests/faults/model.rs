//! The model of the random file tree: what each path must be answered
//! with.

use super::*;

/// The change policy of the static route a path falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Abort,
    AllowGrowth,
}

/// A fault injected into every transfer of a file.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Fault {
    None,
    /// `sendfile` hits end of file at this offset (below the size).
    Shrink(u64),
    /// Chunks read after `after` chunks report the stamp `new`.
    Change {
        after: u64,
        new: FileStamp,
    },
}

#[derive(Debug)]
pub(crate) struct ModelFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) content_type: &'static str,
    pub(crate) fault: Fault,
}

impl ModelFile {
    pub(crate) fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// How a `GET` of this file under `policy` must end if nothing else
    /// (a timeout or reset) cuts it first: `None` when the whole body is
    /// sent, otherwise the abort error and the body bytes received at the
    /// cut (`None` when the cut comes before the head is necessarily out).
    pub(crate) fn expected_abort(&self, policy: Policy) -> Option<(FileError, Option<u64>)> {
        let size = self.size();
        match self.fault {
            Fault::None => None,
            Fault::Shrink(at) => Some((FileError::Truncated, Some(at))),
            Fault::Change { after, new } => {
                if after >= size.div_ceil(CHUNK) {
                    return None;
                }
                let error = match policy {
                    Policy::Abort => FileError::Changed,
                    Policy::AllowGrowth => match new.size.cmp(&size) {
                        std::cmp::Ordering::Greater => return None,
                        std::cmp::Ordering::Equal => FileError::Changed,
                        std::cmp::Ordering::Less => FileError::Truncated,
                    },
                };
                let at = (after > 0).then_some(after * CHUNK);
                Some((error, at))
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Node {
    File(ModelFile),
    Dir,
    /// A symlink, a denied entry or a special file: never served, and
    /// nothing beneath it is reachable.
    Unservable,
}

/// What a static `GET` or `HEAD` without a body must be answered with.
pub(crate) enum Expect<'t> {
    File(&'t ModelFile, Policy),
    Redirect,
    NotFound,
}

/// The test's model of the random file tree under `ROOT`.
#[derive(Debug, Default)]
pub(crate) struct Tree {
    /// Keyed by the relative (decoded) path, e.g. `d/e.txt`.
    pub(crate) nodes: BTreeMap<String, Node>,
    /// Long seeds: a directory with a name long enough that its redirect
    /// may not fit the send buffer.
    pub(crate) long_dir: Option<String>,
}

/// Percent-decodes one path segment (escapes are well-formed: the path
/// passed the target checks).
pub(crate) fn decode(segment: &str) -> String {
    let b = segment.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex = segment.get(i + 1..i + 3).unwrap_or("");
        match (c, u8::from_str_radix(hex, 16)) {
            (b'%', Ok(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path` spells an unreserved character as an escape.
pub(crate) fn has_encoded_unreserved(path: &str) -> bool {
    path.match_indices('%').any(|(i, _)| {
        let hex = path.get(i + 1..i + 3).unwrap_or("");
        u8::from_str_radix(hex, 16)
            .is_ok_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    })
}

/// The change policy and route-relative part of a request path on
/// `example.test`'s static routes. The first segment is compared decoded,
/// as the router does; the rest stays raw.
pub(crate) fn split_route(path: &str) -> (Policy, &str) {
    let after = path.strip_prefix('/').unwrap_or(path);
    let (first, rest) = match after.find('/') {
        Some(i) => (
            after.get(..i).unwrap_or(after),
            after.get(i..).unwrap_or(""),
        ),
        None => (after, ""),
    };
    let first = decode(first);
    for (prefix, policy) in [
        ("g", Policy::AllowGrowth),
        (".well-known", Policy::Abort),
        ("@\u{e4}", Policy::AllowGrowth),
    ] {
        if first == prefix {
            return (policy, rest);
        }
    }
    (Policy::Abort, path)
}

/// Spellings of the `/@%C3%A4` route's segment: as configured, with `@`
/// escaped, and in other hex cases. Only the first is canonical.
pub(crate) const ESCAPED_ROUTE: &[&str] =
    &["@%C3%A4", "%40%C3%A4", "@%c3%a4", "%40%c3%A4", "%40%C3%a4"];

/// Whether `path` reaches the `/@%C3%A4` route through a spelling other
/// than the configured one.
pub(crate) fn is_respelled_route(path: &str) -> bool {
    let first = path
        .strip_prefix('/')
        .and_then(|p| p.split('/').next())
        .unwrap_or("");
    ESCAPED_ROUTE.iter().skip(1).any(|s| *s == first)
}

pub(crate) fn content_type_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some(_) | None => "application/octet-stream",
    }
}

/// File sizes from 0 B to 1 MiB, with the chunk boundaries; small seeds
/// keep to small files so tiny client windows stay cheap.
pub(crate) fn random_size(rng: &mut fastrand::Rng, big: bool) -> u64 {
    if big {
        match rng.u8(0..8) {
            0 => 0,
            1 => CHUNK - 1,
            2 => CHUNK,
            3 => CHUNK + 1,
            4 => 2 * CHUNK,
            5 => MIB,
            _ => rng.u64(0..=MIB),
        }
    } else if rng.u8(0..6) == 0 {
        0
    } else {
        rng.u64(1..1500)
    }
}

/// The contents of a file of `size` bytes, distinct per file and not
/// periodic: a random block repeated (which keeps large files cheap), with
/// each repetition's first four bytes mixed with its index, so every
/// position of the stream is told apart.
pub(crate) fn file_bytes(rng: &mut fastrand::Rng, size: u64) -> Vec<u8> {
    const BLOCK: usize = 509;
    let block: Vec<u8> = (0..BLOCK).map(|_| rng.u8(..)).collect();
    let mut bytes = block.repeat(usize::try_from(size / BLOCK as u64 + 1).unwrap_or(0));
    bytes.truncate(usize::try_from(size).unwrap_or(0));
    for (index, chunk) in bytes.chunks_mut(BLOCK).enumerate() {
        let mark = u32::try_from(index).unwrap_or(u32::MAX).to_le_bytes();
        for (b, m) in chunk.iter_mut().zip(mark) {
            *b ^= m;
        }
    }
    bytes
}

/// A body shifted by any offset must not look like the original, or a
/// response that skipped or repeated bytes could pass the byte-exact check.
#[test]
fn file_contents_are_not_periodic() {
    let mut rng = fastrand::Rng::with_seed(7);
    let bytes = file_bytes(&mut rng, MIB);
    for shift in [
        1,
        2,
        4,
        508,
        509,
        510,
        1018,
        4096,
        509 * 256,
        509 * 512,
        256 * 1024,
    ] {
        let a = bytes.get(..bytes.len() - shift).unwrap_or_default();
        let b = bytes.get(shift..).unwrap_or_default();
        // Not even a window of one chunk matches its shifted self.
        let window = 64 * 1024;
        let same = a.chunks(window).zip(b.chunks(window)).any(|(x, y)| x == y);
        assert!(!same, "a window repeats after a shift of {shift} bytes");
    }
}

pub(crate) fn random_fault(rng: &mut fastrand::Rng, size: u64) -> Fault {
    if size == 0 {
        return Fault::None;
    }
    match rng.u8(0..4) {
        0 => Fault::Shrink(rng.u64(0..size)),
        1 => {
            // `after` may equal the chunk count: the change is never seen.
            let after = rng.u64(0..=size.div_ceil(CHUNK));
            let new_size = match rng.u8(0..4) {
                0 | 1 => size + rng.u64(1..1000),
                2 => size,
                _ => rng.u64(0..size),
            };
            // Far above the sim's own modification times.
            let new = FileStamp {
                size: new_size,
                modified_ns: 1_000_000 + rng.u64(0..1000),
            };
            Fault::Change { after, new }
        }
        _ => Fault::None,
    }
}

impl Tree {
    pub(crate) fn add_parents(&mut self, path: &str) {
        for (i, _) in path.match_indices('/') {
            if let Some(parent) = path.get(..i) {
                self.nodes.entry(parent.to_owned()).or_insert(Node::Dir);
            }
        }
    }

    pub(crate) fn add_dir(&mut self, sim: &mut Sim, path: &str) {
        sim.add_dir(ROOT, path);
        self.add_parents(path);
        self.nodes.insert(path.to_owned(), Node::Dir);
    }

    pub(crate) fn add_file(
        &mut self,
        sim: &mut Sim,
        rng: &mut fastrand::Rng,
        path: &str,
        big: bool,
    ) {
        let size = random_size(rng, big);
        let fault = random_fault(rng, size);
        self.add_file_with(sim, rng, path, size, fault);
    }

    /// A file that always grows while it is sent, before its last chunk:
    /// sent whole under `allow_growth`, aborted under `abort`.
    pub(crate) fn add_growing_file(
        &mut self,
        sim: &mut Sim,
        rng: &mut fastrand::Rng,
        path: &str,
        big: bool,
    ) {
        let size = random_size(rng, big).max(1);
        let new = FileStamp {
            size: size + rng.u64(1..1000),
            modified_ns: 1_000_000 + rng.u64(0..1000),
        };
        let after = rng.u64(0..size.div_ceil(CHUNK));
        self.add_file_with(sim, rng, path, size, Fault::Change { after, new });
    }

    pub(crate) fn add_file_with(
        &mut self,
        sim: &mut Sim,
        rng: &mut fastrand::Rng,
        path: &str,
        size: u64,
        fault: Fault,
    ) {
        let bytes = file_bytes(rng, size);
        sim.add_file(ROOT, path, &bytes);
        match fault {
            Fault::None => {}
            Fault::Shrink(at) => sim.shrink_on_send(ROOT, path, at),
            Fault::Change { after, new } => {
                let after = usize::try_from(after).unwrap_or(usize::MAX);
                sim.change_during_send(ROOT, path, after, new);
            }
        }
        let name = path.rsplit('/').next().unwrap_or_default();
        self.add_parents(path);
        self.nodes.insert(
            path.to_owned(),
            Node::File(ModelFile {
                bytes,
                content_type: content_type_of(name),
                fault,
            }),
        );
    }

    pub(crate) fn add_unservable(&mut self, path: &str) {
        self.add_parents(path);
        self.nodes.insert(path.to_owned(), Node::Unservable);
    }

    /// A random tree in `sim` and its model: files of random sizes and
    /// faults, directories with and without `index.html`, symlinks, a
    /// denied file, a denied directory, a special file and a dotfile.
    pub(crate) fn random(sim: &mut Sim, rng: &mut fastrand::Rng, big: bool, long: bool) -> Tree {
        let mut tree = Tree::default();
        tree.add_dir(sim, "d");
        tree.add_dir(sim, "f");
        tree.add_growing_file(sim, rng, "grow.txt", big);
        if long {
            // Long enough that some redirects with long queries no longer
            // fit the send buffer, short enough for `max_request_line`.
            let dir = format!("l{}", "o".repeat(rng.usize(200..=900)));
            tree.add_dir(sim, &dir);
            if rng.bool() {
                tree.add_file(sim, rng, &format!("{dir}/index.html"), false);
            }
            tree.long_dir = Some(dir);
        }
        for path in [
            "index.html",
            "a.txt",
            "b.bin",
            "c.css",
            "d/index.html",
            "d/e.txt",
            "f/x.bin",
            ".env",
            "d/.hidden",
            "sp ace.txt",
            "\u{e4}.txt",
        ] {
            if rng.u8(0..4) != 0 {
                tree.add_file(sim, rng, path, big);
            }
        }
        if big || rng.u8(0..4) == 0 {
            tree.add_file(sim, rng, "big.bin", big);
        }
        sim.add_symlink(ROOT, "s.txt", "a.txt");
        tree.add_unservable("s.txt");
        if rng.bool() {
            sim.add_file(ROOT, "lnd/x", b"x");
            sim.add_symlink(ROOT, "ln", "lnd");
            tree.add_unservable("ln");
        }
        sim.add_file(ROOT, "secret.txt", b"secret");
        sim.deny(ROOT, "secret.txt");
        tree.add_unservable("secret.txt");
        if rng.bool() {
            sim.add_file(ROOT, "priv/x.txt", b"x");
            sim.deny(ROOT, "priv");
            tree.add_unservable("priv");
        }
        sim.add_special(ROOT, "fifo");
        tree.add_unservable("fifo");
        tree
    }

    /// How a `GET` or `HEAD` without a body of `path` (a request path on
    /// `example.test`) must be answered.
    pub(crate) fn resolve(&self, path: &str) -> Expect<'_> {
        let (policy, rel) = split_route(path);
        let slash = path.ends_with('/');
        let segments: Vec<String> = rel
            .split('/')
            .filter(|s| !s.is_empty())
            .map(decode)
            .collect();
        if segments.iter().any(|s| s.starts_with('.')) {
            return Expect::NotFound;
        }
        let mut key = String::new();
        let mut node = &Node::Dir;
        for segment in segments {
            match node {
                Node::Dir => {}
                Node::File(_) | Node::Unservable => return Expect::NotFound,
            }
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(&segment);
            node = match self.nodes.get(&key) {
                Some(n) => n,
                None => return Expect::NotFound,
            };
        }
        match (node, slash) {
            (Node::File(f), false) => Expect::File(f, policy),
            (Node::Dir, false) => Expect::Redirect,
            (Node::Dir, true) => {
                let index = if key.is_empty() {
                    "index.html".to_owned()
                } else {
                    format!("{key}/index.html")
                };
                match self.nodes.get(&index) {
                    Some(Node::File(f)) => Expect::File(f, policy),
                    Some(Node::Dir | Node::Unservable) | None => Expect::NotFound,
                }
            }
            (Node::File(_), true) | (Node::Unservable, _) => Expect::NotFound,
        }
    }

    /// The status a request logged with `method` and `path` on
    /// `example.test` must get.
    pub(crate) fn expected_status(&self, method: &Method, path: &str) -> Status {
        if path.starts_with("/api/") {
            // The proxy templates (GET, HEAD, POST with a body): the
            // backend refuses.
            return Status::BadGateway;
        }
        match method {
            Method::Get | Method::Head => {}
            Method::Other(_) => return Status::MethodNotAllowed,
        }
        if split_route(path).1.starts_with("/up/") {
            return Status::ContentTooLarge;
        }
        match self.resolve(path) {
            Expect::File(..) => Status::Ok,
            Expect::Redirect => Status::MovedPermanently,
            Expect::NotFound => Status::NotFound,
        }
    }
}
