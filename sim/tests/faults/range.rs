//! Range requests in the static net (FR-STATIC-19): the model of `Range`
//! and `If-Range`, written from the spec and RFC 9110 (sections 13.1.5,
//! 14.1.2, 14.2) and not from the core, the range lines the random clients
//! add, the responses a file request may get by them, and their coverage.

use super::*;

/// What a `GET` of an existing regular file must get by its range, once
/// its conditional headers said it is modified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelRanged {
    Whole,
    /// Bytes `.0` to `.1`, both included: a 206.
    Part(u64, u64),
    Unsatisfiable,
}

/// The one range a `Range` value asks for.
enum ModelSpec {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

/// RFC 9110 section 14.2: `bytes=` (the unit in any case) and a list of
/// exactly one element (empty elements and whitespace around them
/// allowed), `a-b` with `a <= b`, `a-` or `-n`, digits only; `None` for
/// several ranges, another unit or anything malformed.
fn model_spec(value: &str) -> Option<ModelSpec> {
    let (unit, set) = value.split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let elements: Vec<&str> = set
        .split(',')
        .map(|e| e.trim_matches([' ', '\t']))
        .filter(|e| !e.is_empty())
        .collect();
    let [one] = elements.as_slice() else {
        return None;
    };
    let (a, b) = one.split_once('-')?;
    // `Some(None)`: empty; `None`: not a number within u64.
    let num = |s: &str| -> Option<Option<u64>> {
        if s.is_empty() {
            Some(None)
        } else if s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse().ok().map(Some)
        } else {
            None
        }
    };
    match (num(a)?, num(b)?) {
        (Some(a), Some(b)) if a <= b => Some(ModelSpec::FromTo(a, b)),
        (Some(a), None) => Some(ModelSpec::From(a)),
        (None, Some(n)) => Some(ModelSpec::Suffix(n)),
        (Some(_), Some(_)) | (None, None) => None,
    }
}

/// FR-STATIC-19 for a request `head` of a file with `stamp`: only a `GET`
/// with one `Range` line holding one valid range is ranged; an `If-Range`
/// (one line) keeps it only with the file's `ETag` or exactly its
/// `Last-Modified`. A range that overlaps no byte (every range of an
/// empty file, a suffix of 0) is unsatisfiable; one past the end is
/// clamped to it.
pub(crate) fn model_ranged(head: &[u8], stamp: FileStamp) -> ModelRanged {
    if !head.starts_with(b"GET ") {
        return ModelRanged::Whole;
    }
    let ranges = named(head, "range");
    let [value] = ranges.as_slice() else {
        return ModelRanged::Whole;
    };
    let Some(spec) = model_spec(value) else {
        return ModelRanged::Whole;
    };
    let if_range = named(head, "if-range");
    let last_modified = model_imf_fixdate(stamp.modified_ns / 1_000_000_000);
    let kept = match if_range.as_slice() {
        [] => true,
        [one] => *one == model_etag(stamp) || *one == last_modified,
        _ => false,
    };
    if !kept {
        return ModelRanged::Whole;
    }
    let size = stamp.size;
    let (first, last) = match spec {
        ModelSpec::FromTo(a, b) => (a, b.min(size.saturating_sub(1))),
        ModelSpec::From(a) => (a, size.saturating_sub(1)),
        ModelSpec::Suffix(0) => return ModelRanged::Unsatisfiable,
        ModelSpec::Suffix(n) => (size.saturating_sub(n), size.saturating_sub(1)),
    };
    if size == 0 || first >= size {
        ModelRanged::Unsatisfiable
    } else {
        ModelRanged::Part(first, last)
    }
}

/// The status a `GET` or `HEAD` of the existing file `f` must get by the
/// request `head`: a `304` by its conditional headers (they come first,
/// FR-STATIC-19), else a 206 or 416 by its range, else a 200.
pub(crate) fn model_file_status(head: &[u8], f: &ModelFile) -> Status {
    if model_verdict(head, f.stamp, NET_DATE.0) == ModelFreshness::NotModified {
        return Status::NotModified;
    }
    match model_ranged(head, f.stamp) {
        ModelRanged::Whole => Status::Ok,
        ModelRanged::Part(..) => Status::PartialContent,
        ModelRanged::Unsatisfiable => Status::RangeNotSatisfiable,
    }
}

/// The response a logged `status` (a 200, 206 or 416, as
/// [`model_file_status`] gives it) of `f` announces, for the request
/// `head` (`None` when the request is not known: then a 200).
pub(crate) fn file_announced<'t>(
    status: Status,
    f: &'t ModelFile,
    policy: Policy,
    body: BodyMode,
    head: Option<&[u8]>,
) -> Announced<'t> {
    let ranged = head.map_or(ModelRanged::Whole, |h| model_ranged(h, f.stamp));
    // The bytes of the file it sends: none for a 416.
    let span = match ranged {
        ModelRanged::Whole => Some(Span::whole(f.size())),
        ModelRanged::Part(first, last) => Some(Span {
            first,
            end: last + 1,
        }),
        ModelRanged::Unsatisfiable => None,
    };
    let date = Some(NET_DATE.1);
    let renderings = BOTH
        .iter()
        .map(|&persistence| {
            let mut bytes = match ranged {
                ModelRanged::Whole => model_file_head(f.content_type, f.stamp, persistence, date),
                ModelRanged::Part(first, last) => {
                    model_partial_head(f.content_type, f.stamp, (first, last), persistence, date)
                }
                // A fixed response: its text is the body.
                ModelRanged::Unsatisfiable => {
                    model_range_not_satisfiable(f.size(), persistence, date)
                }
            };
            let text = String::from_utf8_lossy(&bytes);
            let head_len = text.find("\r\n\r\n").map_or(bytes.len(), |i| i + 4);
            match (span, body) {
                (Some(span), BodyMode::Full) => {
                    let part = usize::try_from(span.first).unwrap_or(usize::MAX)
                        ..usize::try_from(span.end).unwrap_or(usize::MAX);
                    bytes.extend_from_slice(f.bytes.get(part).unwrap_or_default());
                }
                (None, BodyMode::Full) => {}
                (Some(_) | None, BodyMode::HeadOnly) => bytes.truncate(head_len),
            }
            Rendering {
                persistence,
                with_query: false,
                head_len,
                bytes,
            }
        })
        .collect();
    let file = span.map(|span| (f, policy, span));
    Announced {
        status,
        body,
        renderings,
        file,
        stamp: Some(f.stamp),
    }
}

/// `Range` (and sometimes `If-Range`) lines for a request of the file with
/// `stamp` (or of no file), in about two requests in three: valid ranges
/// inside, past and across the file's end and its chunks, suffixes, open
/// ends, unsatisfiable ones, several ranges, malformed ones, another unit,
/// two lines; `If-Range` with the file's tag, a stale or weak one, its
/// `Last-Modified`, a second later or in RFC 850 form. Short, as the
/// conditional lines are: under the short limits the head must fit 160
/// bytes.
pub(crate) fn random_range(rng: &mut fastrand::Rng, stamp: Option<FileStamp>) -> String {
    if rng.u8(0..3) == 0 {
        return String::new();
    }
    let stamp = stamp.unwrap_or(FileStamp {
        size: rng.u64(0..2000),
        modified_ns: rng.u64(0..=NET_DATE.0) * 1_000_000_000,
    });
    let size = stamp.size;
    let inside = |rng: &mut fastrand::Rng| rng.u64(0..size.max(1));
    // A file of more than two chunks: half the time a range longer than
    // a chunk, so ranges read in several chunks are common.
    // Else a quarter of the time one of the kinds held to
    // `MIN_RANGE_KIND`: past the end, open, a suffix, several ranges.
    let draw = if size > 2 * CHUNK && rng.bool() {
        6
    } else if rng.u8(0..4) == 0 {
        rng.choice([3, 4, 5, 10]).unwrap_or_default()
    } else {
        rng.u8(0..16)
    };
    let spec = match draw {
        0..3 => {
            let a = inside(rng);
            format!("bytes={a}-{}", rng.u64(a..size.max(a + 1)))
        }
        // Past the end: clamped.
        3 => format!("bytes={}-{}", inside(rng), size + rng.u64(0..100)),
        4 => format!("bytes={}-", inside(rng)),
        5 => format!("bytes=-{}", rng.u64(1..size.max(1) + 2)),
        // Longer than a chunk: read in two or more (inside a file of more
        // than a chunk, else clamped or unsatisfiable).
        6 if size > CHUNK + 1 => {
            let a = rng.u64(0..size - CHUNK - 1);
            format!("bytes={a}-{}", rng.u64(a + CHUNK..size))
        }
        6 => format!("bytes={}-{}", rng.u64(0..CHUNK), 2 * CHUNK),
        7 => format!("BYTES= {}-{} ,", inside(rng), size.saturating_sub(1)),
        // Unsatisfiable.
        8 => format!("bytes={}-", size + rng.u64(0..10)),
        9 => "bytes=-0".to_owned(),
        // Several ranges, malformed ones, another unit: the whole file.
        10 => "bytes=0-1,5-6".to_owned(),
        11 => rng
            .choice([
                "bytes=5-1",
                "bytes=0x-1",
                "bytes 0-1",
                "bytes=1-2-3",
                "bytes=,",
            ])
            .unwrap_or_default()
            .to_owned(),
        12 => "items=0-1".to_owned(),
        13 => "bytes=18446744073709551616-".to_owned(),
        14 => "bytes=0-0\r\nRange: bytes=1-1".to_owned(),
        _ => format!("bytes=0-{}", rng.u64(0..300_000)),
    };
    let etag = model_etag(stamp);
    let mtime = stamp.modified_ns / 1_000_000_000;
    let if_range = match rng.u8(0..10) {
        0 | 1 => format!("If-Range: {etag}\r\n"),
        2 => {
            let stale = FileStamp {
                size: stamp.size + 1,
                ..stamp
            };
            format!("If-Range: {}\r\n", model_etag(stale))
        }
        3 => format!("If-Range: W/{etag}\r\n"),
        4 => format!("If-Range: {}\r\n", model_imf_fixdate(mtime)),
        5 => format!("If-Range: {}\r\n", model_imf_fixdate(mtime + 1)),
        6 => {
            let [_, rfc850, _] = date_forms(mtime);
            format!("If-Range: {rfc850}\r\n")
        }
        _ => String::new(),
    };
    format!("Range: {spec}\r\n{if_range}")
}

/// Mixed into the generator's state to seed [`long_range_request`].
const LONG_RANGE_STREAM: u64 = 0x4c4f_4e47_5241_4e47;

/// One request in six, when the tree has a file of more than one chunk
/// that a plain path reaches (big seeds): a `GET` of a range of it longer
/// than a chunk, under `/` or `/g/`, from a stream seeded by `seed`.
pub(crate) fn long_range_request(seed: u64, tree: &Tree) -> Option<Vec<u8>> {
    let mut rng = fastrand::Rng::with_seed(seed ^ LONG_RANGE_STREAM);
    if rng.u8(0..6) != 0 {
        return None;
    }
    let files: Vec<(&str, u64)> = tree
        .nodes
        .iter()
        .filter_map(|(key, node)| match node {
            Node::File(f) if f.size() > CHUNK + 1 && plain(key) => Some((key.as_str(), f.size())),
            Node::File(_) | Node::Dir | Node::Unservable => None,
        })
        .collect();
    let (path, size) = rng.choice(files)?;
    let first = rng.u64(0..size - CHUNK - 1);
    let last = rng.u64(first + CHUNK..size);
    let prefix = if rng.bool() { "/" } else { "/g/" };
    let close = if rng.u8(0..6) == 0 {
        "Connection: close\r\n"
    } else {
        ""
    };
    Some(
        format!(
            "GET {prefix}{path} HTTP/1.1\r\nHost: example.test\r\n{close}\
             Range: bytes={first}-{last}\r\n\r\n"
        )
        .into_bytes(),
    )
}

/// Mixed into the generator's state to seed [`kind_range_request`].
const KIND_RANGE_STREAM: u64 = 0x4b49_4e44_5241_4e47;

/// One request in `KIND_RANGE_ONE_IN`, when the tree has a non-empty file
/// that a plain path reaches: a `GET` of it, under `/` or `/g/`, with a
/// range of one of the kinds gated by `MIN_RANGE_KIND`, which
/// [`random_range`]'s lines reach too rarely on their own: past the end
/// (clamped), open, a suffix, or several ranges (the whole file). No
/// `If-Range`; from a stream seeded by `seed`, `None` when the head would
/// not fit `tree.max_head`.
pub(crate) fn kind_range_request(seed: u64, tree: &Tree) -> Option<Vec<u8>> {
    let mut rng = fastrand::Rng::with_seed(seed ^ KIND_RANGE_STREAM);
    if rng.u8(0..KIND_RANGE_ONE_IN) != 0 {
        return None;
    }
    let files: Vec<(&str, u64)> = tree
        .nodes
        .iter()
        .filter_map(|(key, node)| match node {
            Node::File(f) if f.size() > 0 && plain(key) => Some((key.as_str(), f.size())),
            Node::File(_) | Node::Dir | Node::Unservable => None,
        })
        .collect();
    let (path, size) = rng.choice(files)?;
    let spec = match rng.u8(0..4) {
        0 => format!("bytes={}-{}", rng.u64(0..size), size + rng.u64(0..100)),
        1 => format!("bytes={}-", rng.u64(0..size)),
        2 => format!("bytes=-{}", rng.u64(1..size + 2)),
        _ => "bytes=0-1,5-6".to_owned(),
    };
    let prefix = if rng.bool() { "/" } else { "/g/" };
    let close = if rng.u8(0..6) == 0 {
        "Connection: close\r\n"
    } else {
        ""
    };
    let head = format!(
        "GET {prefix}{path} HTTP/1.1\r\nHost: example.test\r\n{close}Range: {spec}\r\n\r\n"
    );
    (head.len() <= tree.max_head - 8).then(|| head.into_bytes())
}

/// How rarely [`kind_range_request`] stands in for a drawn request.
const KIND_RANGE_ONE_IN: u8 = 4;

/// Whether the tree's `key` is reached by its path as it is: no escapes,
/// no dot segment.
fn plain(key: &str) -> bool {
    key.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"./_-".contains(&b))
        && !key.split('/').any(|s| s.starts_with('.'))
}

/// What the range requests reached, summed over all seeds.
#[derive(Debug, Default)]
pub(crate) struct RangeCoverage {
    counts: BTreeMap<&'static str, usize>,
}

/// Every name [`RangeCoverage::count`] can give; each must be reached.
const RANGE_NAMES: [&str; 13] = [
    "range-206",
    "range-suffix",
    "range-open-end",
    "range-clamped",
    "range-416",
    "range-multi-200",
    "range-malformed-200",
    "if-range-206",
    "if-range-200",
    "range-304",
    "range-head-200",
    "range-across-chunk",
    "range-cut",
];

/// `range-across-chunk` must be well clear of zero (like `MIN_GROWN`).
const MIN_ACROSS_CHUNK: usize = 15;

/// The kinds [`kind_range_request`] draws must be well clear of zero too.
const MIN_RANGE_KIND: usize = 15;

/// The names held to `MIN_RANGE_KIND`.
const RANGE_KINDS: [&str; 4] = [
    "range-clamped",
    "range-open-end",
    "range-suffix",
    "range-multi-200",
];

impl RangeCoverage {
    fn hit(&mut self, name: &'static str) {
        *self.counts.entry(name).or_default() += 1;
    }

    /// A 206 of more than one chunk whose bytes were checked past its
    /// first chunk: sent whole, or cut later.
    pub(crate) fn across_chunk(&mut self) {
        self.hit("range-across-chunk");
    }

    /// A 206 cut by the fault its file must abort with, exactly where the
    /// fault strikes within the range (`ModelFile::expected_abort`).
    pub(crate) fn cut(&mut self) {
        self.hit("range-cut");
    }

    /// Count a complete response of `status` to the file request `head`
    /// for a file with `stamp`.
    pub(crate) fn count(&mut self, head: &[u8], stamp: FileStamp, status: Status) {
        let ranges = named(head, "range");
        let Some(value) = ranges.first() else {
            return;
        };
        let if_range = !named(head, "if-range").is_empty();
        let spec = model_spec(value);
        let get = head.starts_with(b"GET ");
        match status {
            Status::PartialContent => {
                self.hit("range-206");
                if if_range {
                    self.hit("if-range-206");
                }
                match spec {
                    Some(ModelSpec::Suffix(_)) => self.hit("range-suffix"),
                    Some(ModelSpec::From(_)) => self.hit("range-open-end"),
                    Some(ModelSpec::FromTo(_, b)) if b >= stamp.size => self.hit("range-clamped"),
                    Some(ModelSpec::FromTo(..)) | None => {}
                }
                if let ModelRanged::Part(first, last) = model_ranged(head, stamp) {
                    if last - first + 1 > CHUNK {
                        self.across_chunk();
                    }
                }
            }
            Status::RangeNotSatisfiable => self.hit("range-416"),
            Status::NotModified => self.hit("range-304"),
            Status::Ok if !get => self.hit("range-head-200"),
            Status::Ok if ranges.len() > 1 || value.starts_with("bytes=0-1,") => {
                self.hit("range-multi-200");
            }
            Status::Ok if spec.is_none() => self.hit("range-malformed-200"),
            Status::Ok if if_range => self.hit("if-range-200"),
            Status::Ok
            | Status::MovedPermanently
            | Status::BadRequest
            | Status::NotFound
            | Status::MethodNotAllowed
            | Status::RequestTimeout
            | Status::ContentTooLarge
            | Status::UriTooLong
            | Status::ExpectationFailed
            | Status::RequestHeaderFieldsTooLarge
            | Status::BadGateway
            | Status::ServiceUnavailable
            | Status::GatewayTimeout
            | Status::HttpVersionNotSupported => {}
        }
    }

    pub(crate) fn check(&self) {
        let across = self.counts.get("range-across-chunk").copied().unwrap_or(0);
        assert!(
            across >= MIN_ACROSS_CHUNK,
            "only {across} 206s checked past their first chunk (want {MIN_ACROSS_CHUNK})"
        );
        for name in RANGE_KINDS {
            let n = self.counts.get(name).copied().unwrap_or(0);
            assert!(
                n >= MIN_RANGE_KIND,
                "only {n} {name} in all seeds (want {MIN_RANGE_KIND}): {:?}",
                self.counts
            );
        }
        for name in RANGE_NAMES {
            assert!(
                self.counts.get(name).is_some_and(|n| *n > 0),
                "no {name} in any seed: {:?}",
                self.counts
            );
        }
    }
}

#[test]
fn the_model_judges_ranges_from_the_spec() {
    let stamp = FileStamp {
        size: 1000,
        modified_ns: 784_111_777_000_000_000,
    };
    let get =
        |lines: &str| model_ranged(format!("GET / HTTP/1.1\r\n{lines}\r\n").as_bytes(), stamp);
    assert_eq!(get("Range: bytes=0-99\r\n"), ModelRanged::Part(0, 99));
    assert_eq!(get("Range: bytes=-2000\r\n"), ModelRanged::Part(0, 999));
    assert_eq!(
        get("Range: bytes=990-5000\r\n"),
        ModelRanged::Part(990, 999)
    );
    assert_eq!(get("Range: bytes=1000-\r\n"), ModelRanged::Unsatisfiable);
    assert_eq!(get("Range: bytes=-0\r\n"), ModelRanged::Unsatisfiable);
    assert_eq!(get("Range: bytes=0-1,5-6\r\n"), ModelRanged::Whole);
    assert_eq!(
        get("Range: bytes=0-9\r\nIf-Range: Sun, 06 Nov 1994 08:49:37 GMT\r\n"),
        ModelRanged::Part(0, 9)
    );
    assert_eq!(
        get("Range: bytes=0-9\r\nIf-Range: W/\"3e8-ae1b981bc490a00\"\r\n"),
        ModelRanged::Whole
    );
    assert_eq!(
        get("Range: bytes=0-9\r\nIf-Range: \"3e8-ae1b981bc490a00\"\r\n"),
        ModelRanged::Part(0, 9)
    );
}
