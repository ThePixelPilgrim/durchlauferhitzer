//! Backend-directed files in the proxy net (FR-PROXY-32, FR-STATIC-19),
//! what is drawn: each `internal_files` root gets a random file tree in
//! the sim's file system (regular files of 0 to 600 KiB, directories,
//! dotfiles, symlinks, denied and missing entries, files that shrink or
//! change while they are sent); clients add conditional and range lines
//! for a file of it; backends answer `200`s naming files by the route's
//! header (valid paths, every bad-path class, traversal attempts, the
//! header twice), with any body framing. The model of what each must
//! give is `file_model.rs`.

use super::*;

/// The size of the largest file.
const MAX_FILE: u64 = 600 * 1024;

/// An entry of a root's tree, as the model sees it.
#[derive(Debug)]
pub(crate) enum Entry {
    File(Rc<ModelFile>),
    Dir,
    /// A symlink, a denied entry or a special file: never served, and
    /// nothing beneath it is reachable (FR-STATIC-09).
    Unservable,
}

/// How an unservable entry is made in the sim.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Unservable {
    Symlink(&'static str),
    DeniedFile,
    /// A denied directory holding `x.txt`.
    DeniedDir,
    Special,
}

/// A random tree below one `internal_files` root, and its model.
#[derive(Debug)]
pub(crate) struct FileTree {
    pub(crate) root: &'static str,
    /// Keyed by the path below the root, e.g. `d/e.html`.
    pub(crate) nodes: BTreeMap<String, Entry>,
    pub(crate) unservable: Vec<(&'static str, Unservable)>,
}

/// The regular files a tree may hold (each present three times in four)
/// and dotfiles; names with a space and a `%` among them.
const FILES: &[&str] = &[
    "a.txt",
    "b.pdf",
    "c.json",
    "big.bin",
    "x",
    "d/e.html",
    "d/f/g.css",
    "sp ace.txt",
    "100%.png",
    ".env",
    "d/.hidden",
    ".git/config",
];

/// Names a backend may draw that are never files: directories, the
/// unservable entries, missing names.
const OTHER: &[&str] = &[
    "d",
    "d/f",
    "empty",
    "s.txt",
    "ln",
    "ln/e.html",
    "secret.txt",
    "priv",
    "priv/x.txt",
    "fifo",
    "nope.txt",
    "d/nope",
    "a.txt/x",
];

/// FR-STATIC-12: `Content-Type` from the last `.` of the file name.
fn static_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// A size from 0 to [`MAX_FILE`], the chunk boundaries among them;
/// `big` files always span more than a chunk.
fn file_size(rng: &mut fastrand::Rng, big: bool) -> u64 {
    if big {
        return match rng.u8(0..4) {
            0 => CHUNK + 1,
            1 => 2 * CHUNK,
            _ => rng.u64(CHUNK + 2..=MAX_FILE),
        };
    }
    match rng.u8(0..10) {
        0 => 0,
        1 => CHUNK - 1,
        2 => CHUNK,
        3 => rng.u64(1500..=MAX_FILE),
        _ => rng.u64(1..1500),
    }
}

impl FileTree {
    /// A random tree below `root`.
    pub(crate) fn random(rng: &mut fastrand::Rng, root: &'static str) -> FileTree {
        let mut tree = FileTree {
            root,
            nodes: BTreeMap::new(),
            unservable: Vec::new(),
        };
        for dir in ["d", "d/f", "empty", ".git"] {
            tree.nodes.insert(dir.to_owned(), Entry::Dir);
        }
        for &path in FILES {
            if rng.u8(0..4) == 0 {
                continue;
            }
            let size = file_size(rng, path == "big.bin");
            let bytes = file_bytes(rng, size);
            // An mtime up to a day before the net's clock.
            let secs = NET_DATE.0 - rng.u64(0..86_400);
            let stamp = FileStamp {
                size,
                modified_ns: secs * 1_000_000_000 + rng.u64(0..1_000_000_000),
            };
            let name = path.rsplit('/').next().unwrap_or(path);
            let file = ModelFile {
                bytes,
                stamp,
                content_type: static_type(name),
                fault: random_fault(rng, size),
            };
            tree.nodes
                .insert(path.to_owned(), Entry::File(Rc::new(file)));
        }
        for (path, kind) in [
            ("s.txt", Unservable::Symlink("a.txt")),
            ("ln", Unservable::Symlink("d")),
            ("secret.txt", Unservable::DeniedFile),
            ("priv", Unservable::DeniedDir),
            ("fifo", Unservable::Special),
        ] {
            tree.nodes.insert(path.to_owned(), Entry::Unservable);
            tree.unservable.push((path, kind));
        }
        tree
    }

    /// Makes the tree in `sim`'s file system.
    pub(crate) fn install(&self, sim: &mut Sim) {
        let root = self.root;
        for (path, entry) in &self.nodes {
            match entry {
                Entry::Dir => sim.add_dir(root, path),
                Entry::File(f) => {
                    sim.add_file(root, path, &f.bytes);
                    sim.set_modified(root, path, f.stamp.modified_ns);
                    match f.fault {
                        Fault::None => {}
                        Fault::Shrink(at) => sim.shrink_on_send(root, path, at),
                        Fault::Change { after, new } => {
                            let after = usize::try_from(after).unwrap_or(usize::MAX);
                            sim.change_during_send(root, path, after, new);
                        }
                    }
                }
                Entry::Unservable => {}
            }
        }
        for &(path, kind) in &self.unservable {
            match kind {
                Unservable::Symlink(target) => sim.add_symlink(root, path, target),
                Unservable::DeniedFile => {
                    sim.add_file(root, path, b"secret");
                    sim.deny(root, path);
                }
                Unservable::DeniedDir => {
                    sim.add_file(root, &format!("{path}/x.txt"), b"x");
                    sim.deny(root, path);
                }
                Unservable::Special => sim.add_special(root, path),
            }
        }
    }
}

/// The trees of a net's routes (one per root), and each route's.
pub(crate) fn route_trees(
    rng: &mut fastrand::Rng,
    files: [Option<RouteFiles>; 2],
) -> [Option<Rc<FileTree>>; 2] {
    let mut trees: Vec<Rc<FileTree>> = Vec::new();
    files.map(|f| {
        let root = f.map(RouteFiles::root)?;
        if let Some(t) = trees.iter().find(|t| t.root == root) {
            return Some(Rc::clone(t));
        }
        let tree = Rc::new(FileTree::random(rng, root));
        trees.push(Rc::clone(&tree));
        Some(tree)
    })
}

// ---- clients ---------------------------------------------------------------

/// Header lines: names and values.
pub(crate) type HeaderLines = Vec<(String, Vec<u8>)>;

/// On an `internal_files` route, three requests in four name a file of the
/// tree they "want" (the backend names it most of the time) and carry
/// conditional and range lines for it (`conditional.rs`, `range.rs`):
/// the file's path and the lines.
pub(crate) fn wish_lines(
    rng: &mut fastrand::Rng,
    tree: Option<&FileTree>,
) -> Option<(String, HeaderLines)> {
    let tree = tree?;
    if rng.u8(0..4) == 0 {
        return None;
    }
    let files: Vec<(&String, &Rc<ModelFile>)> = tree
        .nodes
        .iter()
        .filter_map(|(k, e)| match e {
            Entry::File(f) => Some((k, f)),
            Entry::Dir | Entry::Unservable => None,
        })
        .collect();
    let (path, file) = rng.choice(files)?;
    let mut text = random_conditional(rng, Some(file.stamp));
    let size = file.size();
    match rng.u8(0..6) {
        // Past the end: a 416 (unless a 304 comes first).
        0 => text.push_str(&format!("Range: bytes={}-\r\n", size + rng.u64(0..3))),
        // Inside the file: a 206.
        1 if size > 0 => {
            let first = rng.u64(0..size);
            text.push_str(&format!(
                "Range: bytes={first}-{}\r\n",
                rng.u64(first..size)
            ));
        }
        _ => text.push_str(&random_range(rng, Some(file.stamp))),
    }
    let lines = text
        .split("\r\n")
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_owned(), v.as_bytes().to_vec()))
        .collect();
    Some((path.clone(), lines))
}

// ---- backends --------------------------------------------------------------

/// The header values a backend's `200` names a file with (one, or two
/// lines of the route's header).
#[derive(Debug, Clone)]
pub(crate) struct Trigger {
    pub(crate) values: Vec<Vec<u8>>,
}

impl Trigger {
    /// A value holds obs-text: the response head parser refuses the head
    /// (FR-PROXY-13, a `502` `BadResponse(BadHeaderValue)`) before any
    /// path rule is applied.
    pub(crate) fn obs_text(&self) -> bool {
        self.values.iter().flatten().any(|b| *b >= 0x80)
    }
}

/// Whether a drawn value may hold obs-text: not in a head that carries a
/// defect of its own (which of the two the parser reports first is not
/// what this net checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObsText {
    Allowed,
    Never,
}

/// Percent-encodes `path` for `X-Accel-Redirect`: `%` and the space
/// always, other bytes now and then, in either hex case.
fn encode(rng: &mut fastrand::Rng, path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b == b'%' || b == b' ' || (b != b'/' && rng.u8(0..8) == 0) {
            if rng.bool() {
                out.push_str(&format!("%{b:02X}"));
            } else {
                out.push_str(&format!("%{b:02x}"));
            }
        } else {
            out.push(char::from(b));
        }
    }
    out
}

/// A value naming `path` (below the root) for `files`, maybe spoilt.
fn value_for(rng: &mut fastrand::Rng, files: RouteFiles, path: &str, obs: ObsText) -> Vec<u8> {
    let named = match files {
        RouteFiles::XSendfile { root } => match rng.u8(0..20) {
            0 => root.to_owned(),
            _ => format!("{root}/{path}"),
        },
        RouteFiles::XAccelRedirect { prefix, .. } => format!("{prefix}{}", encode(rng, path)),
    };
    let spoilt = match rng.u8(0..48) {
        0 => String::new(),
        1 => format!("{named}?v=1"),
        2 => named.replacen('/', "/ ", 2),
        3 => named.replacen('/', "\t/", 2),
        4 => format!("{named}/./x"),
        5 => format!("{named}/../../etc/passwd"),
        6 => named.replacen('/', "//", 2),
        7 => format!("{named}/.."),
        8 => match files {
            RouteFiles::XSendfile { root } => format!("{root}x/{path}"),
            RouteFiles::XAccelRedirect { .. } => format!("/elsewhere/{path}"),
        },
        9 => match files {
            RouteFiles::XSendfile { .. } => format!("/etc/../{named}"),
            RouteFiles::XAccelRedirect { prefix, .. } => {
                format!("{prefix}%2e%2e/{}", encode(rng, path))
            }
        },
        10 => match files {
            RouteFiles::XSendfile { .. } => format!("{named}/"),
            RouteFiles::XAccelRedirect { prefix, .. } => format!("{prefix}..%2f{path}"),
        },
        11 => match files {
            RouteFiles::XSendfile { .. } => format!("{named}%2e"),
            RouteFiles::XAccelRedirect { .. } => format!("{named}%zz"),
        },
        12 => match files {
            RouteFiles::XSendfile { .. } => named.to_ascii_uppercase(),
            RouteFiles::XAccelRedirect { .. } => format!("{named}%4"),
        },
        13 => match files {
            RouteFiles::XSendfile { .. } => "/etc/passwd".to_owned(),
            RouteFiles::XAccelRedirect { prefix, .. } => format!("{prefix}a%00b"),
        },
        // The root or the prefix alone (an empty rest): a directory, 404.
        14 => match files {
            RouteFiles::XSendfile { root } => format!("{root}/"),
            RouteFiles::XAccelRedirect { prefix, .. } => prefix.to_owned(),
        },
        // obs-text (a byte outside visible ASCII the parser refuses).
        15 | 16 if obs == ObsText::Allowed => {
            let mut v = named.into_bytes();
            let at = v.len() / 2;
            v.splice(at..at, *b"\xc3\xa9");
            return v;
        }
        _ => return named.into_bytes(),
    };
    spoilt.into_bytes()
}

/// A random trigger for `files`: mostly the client's `wish`, else any
/// name; one time in twelve, two lines.
pub(crate) fn random_trigger(
    rng: &mut fastrand::Rng,
    files: RouteFiles,
    wish: Option<&str>,
    obs: ObsText,
) -> Trigger {
    let pick = |rng: &mut fastrand::Rng| {
        let path = match wish {
            Some(w) if rng.u8(0..6) != 0 => w,
            Some(_) | None if rng.bool() => rng.choice(FILES).copied().unwrap_or("a.txt"),
            Some(_) | None => rng.choice(OTHER).copied().unwrap_or("d"),
        };
        value_for(rng, files, path, obs)
    };
    let mut values = vec![pick(rng)];
    if rng.u8(0..12) == 0 {
        values.push(pick(rng));
    }
    Trigger { values }
}

/// Header lines a backend may send with a file: kept, dropped
/// (replaced by the file's), or ruling out the core's own.
pub(crate) const FILE_EXTRA: &[(&str, &[u8])] = &[
    ("Content-Disposition", b"attachment; filename=\"f\""),
    ("Cache-Control", b"private"),
    ("ETag", b"\"backend\""),
    ("Last-Modified", b"Tue, 27 Oct 2026 10:00:00 GMT"),
    ("Accept-Ranges", b"none"),
    ("Content-Range", b"bytes 0-1/2"),
    ("X-Content-Type-Options", b"nosniff"),
    ("Content-Type", b"application/x-backend"),
];
