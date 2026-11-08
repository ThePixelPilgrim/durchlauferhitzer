use super::*;
use proptest::prelude::*;

fn open(segments: &[&[u8]], slash: TrailingSlash) -> Result<Located, BadPath> {
    Ok(Located::Open {
        segments: segments.iter().map(|s| s.to_vec()).collect(),
        slash,
    })
}

const ROOT: &[u8] = b"/srv/files";
const PREFIX: &[u8] = b"/protected/";

#[test]
fn sendfile_table() {
    use TrailingSlash::{Absent, Present};
    let cases: [(&[u8], Result<Located, BadPath>); 19] = [
        (b"/srv/files/a/b.pdf", open(&[b"a", b"b.pdf"], Absent)),
        (b"/srv/files/a%20b", open(&[b"a%20b"], Absent)),
        (b"/srv/files/", open(&[], Present)),
        (b"/srv/files", open(&[], Absent)),
        (b"/srv/files/a/", open(&[b"a"], Present)),
        (b"/srv/filesx/a", Err(BadPath::NoMatch)),
        (b"/srv/files-old/a", Err(BadPath::NoMatch)),
        (b"/etc/passwd", Err(BadPath::NoMatch)),
        (b"", Err(BadPath::Empty)),
        (b"/srv/files/a b", Err(BadPath::BadByte)),
        (b"/srv/files/\x7f", Err(BadPath::BadByte)),
        (b"/srv/files/\xc3\xa4", Err(BadPath::BadByte)),
        (b"/srv/files/a?x", Err(BadPath::Query)),
        (b"/srv/files//a", Err(BadPath::BadSegment)),
        (b"/srv/files/./a", Err(BadPath::BadSegment)),
        (b"/srv/files/a/../b", Err(BadPath::BadSegment)),
        (b"/srv/files/a//", Err(BadPath::BadSegment)),
        (b"/srv/files/.env", Ok(Located::NotFound)),
        (b"/srv/files/a/.git/x", Ok(Located::NotFound)),
    ];
    for (value, expected) in cases {
        assert_eq!(
            sendfile_path(ROOT, value),
            expected,
            "{}",
            value.escape_ascii()
        );
    }
}

#[test]
fn accel_table() {
    use TrailingSlash::{Absent, Present};
    let cases: [(&[u8], Result<Located, BadPath>); 17] = [
        (b"/protected/a/b.pdf", open(&[b"a", b"b.pdf"], Absent)),
        (b"/protected/a%20b", open(&[b"a b"], Absent)),
        (b"/protected/%C3%A4", open(&[&[0xC3, 0xA4]], Absent)),
        (b"/protected/%252e", open(&[b"%2e"], Absent)),
        (b"/protected/", open(&[], Absent)),
        (b"/protected/a/", open(&[b"a"], Present)),
        (b"/protected", Err(BadPath::NoMatch)),
        (b"/Protected/a", Err(BadPath::NoMatch)),
        (b"/other/a", Err(BadPath::NoMatch)),
        (b"/protected/%zz", Err(BadPath::BadEscape)),
        (b"/protected/%2", Err(BadPath::BadEscape)),
        (b"/protected/%2e%2e/x", Ok(Located::NotFound)),
        (b"/protected/a%2Fb", Ok(Located::NotFound)),
        (b"/protected/%00", Ok(Located::NotFound)),
        (b"/protected/../x", Err(BadPath::BadSegment)),
        (b"/protected//x", Err(BadPath::BadSegment)),
        (b"/protected/a?b", Err(BadPath::Query)),
    ];
    for (value, expected) in cases {
        assert_eq!(
            accel_path(PREFIX, value),
            expected,
            "{}",
            value.escape_ascii()
        );
    }
}

/// The checks run in order: each earlier failure wins over a later one
/// found in the same value.
#[test]
fn check_order() {
    // A bad byte beats a query and a mismatch.
    assert_eq!(sendfile_path(ROOT, b"/etc/a?b c"), Err(BadPath::BadByte));
    // A query beats a mismatch and a bad segment.
    assert_eq!(sendfile_path(ROOT, b"/etc/../a?"), Err(BadPath::Query));
    assert_eq!(accel_path(PREFIX, b"/other/?"), Err(BadPath::Query));
    // A mismatch beats a bad segment.
    assert_eq!(sendfile_path(ROOT, b"/etc/../x"), Err(BadPath::NoMatch));
    // A raw `..` anywhere beats a bad escape and a dotfile before it.
    assert_eq!(
        accel_path(PREFIX, b"/protected/%zz/.x/.."),
        Err(BadPath::BadSegment)
    );
    assert_eq!(
        sendfile_path(ROOT, b"/srv/files/.git/.."),
        Err(BadPath::BadSegment)
    );
    // A bad escape anywhere beats a dotfile before it.
    assert_eq!(
        accel_path(PREFIX, b"/protected/.x/%zz"),
        Err(BadPath::BadEscape)
    );
    assert_eq!(
        accel_path(PREFIX, b"/protected/%2e%2e/%4"),
        Err(BadPath::BadEscape)
    );
}

/// The raw segment check sees the undecoded segments: an escaped `.` or
/// `..` is a dotfile (404), not a bad path.
#[test]
fn escaped_dots_are_dotfiles() {
    for value in [
        &b"/protected/%2e"[..],
        b"/protected/%2E%2e",
        b"/protected/a/%2e%2e/b",
        b"/protected/%2ehidden",
    ] {
        assert_eq!(
            accel_path(PREFIX, value),
            Ok(Located::NotFound),
            "{}",
            value.escape_ascii()
        );
    }
}

#[test]
fn sendfile_values_are_not_decoded() {
    assert_eq!(
        sendfile_path(ROOT, b"/srv/files/%2e%2e/x"),
        open(&[b"%2e%2e", b"x"], TrailingSlash::Absent)
    );
    assert_eq!(
        sendfile_path(ROOT, b"/srv/files/%zz"),
        open(&[b"%zz"], TrailingSlash::Absent)
    );
}

#[test]
fn accel_prefix_slash_alone() {
    assert_eq!(
        accel_path(b"/", b"/a/b"),
        open(&[b"a", b"b"], TrailingSlash::Absent)
    );
    assert_eq!(accel_path(b"/", b"/"), open(&[], TrailingSlash::Absent));
    assert_eq!(accel_path(b"/", b"//a"), Err(BadPath::BadSegment));
    assert_eq!(accel_path(b"/", b"a"), Err(BadPath::NoMatch));
}

#[test]
fn bad_byte_bounds() {
    for value in [
        &b"/srv/files/\x20"[..],
        b"/srv/files/\x00",
        b"/srv/files/\x80",
        b"/srv/files/\x09",
        b"/srv/files/\xff",
    ] {
        assert_eq!(
            sendfile_path(ROOT, value),
            Err(BadPath::BadByte),
            "{}",
            value.escape_ascii()
        );
    }
    assert_eq!(
        sendfile_path(ROOT, b"/srv/files/!~"),
        open(&[b"!~"], TrailingSlash::Absent)
    );
}

/// Bytes biased towards the characters the parser cares about, so random
/// values reach every check.
fn value_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        4 => Just(b'/'),
        3 => Just(b'.'),
        2 => Just(b'%'),
        1 => Just(b'?'),
        2 => prop_oneof![
            Just(b'2'),
            Just(b'e'),
            Just(b'E'),
            Just(b'f'),
            Just(b'F'),
            Just(b'0'),
        ],
        3 => b'a'..=b'c',
        1 => any::<u8>(),
    ]
}

fn tail() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(value_byte(), 0..24)
}

fn with_start(start: &'static [u8]) -> impl Strategy<Value = Vec<u8>> {
    tail().prop_map(move |t| [start, &t[..]].concat())
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => with_start(b"/srv/files/"),
        1 => with_start(b"/srv/files"),
        3 => with_start(b"/protected/"),
        1 => tail(),
        1 => proptest::collection::vec(any::<u8>(), 0..32),
    ]
}

/// What every `Open` must satisfy, whatever the input.
fn check_segments(located: &Located) -> Result<(), TestCaseError> {
    match located {
        Located::Open { segments, .. } => {
            for segment in segments {
                prop_assert!(!segment.is_empty());
                prop_assert!(!segment.contains(&b'/'), "{:?}", segment);
                prop_assert!(!segment.contains(&0), "{:?}", segment);
                prop_assert!(segment != b"." && segment != b"..");
                prop_assert!(segment.first() != Some(&b'.'), "{:?}", segment);
            }
            Ok(())
        }
        Located::NotFound => Ok(()),
    }
}

fn visible(value: &[u8]) -> bool {
    value.iter().all(|b| (0x21..=0x7E).contains(b))
}

proptest! {
    #[test]
    fn sendfile_open_is_safe(value in value()) {
        if let Ok(located) = sendfile_path(ROOT, &value) {
            check_segments(&located)?;
            match located {
                Located::Open { segments, .. } => {
                    prop_assert!(
                        value == ROOT || value.starts_with(b"/srv/files/"),
                        "{}",
                        value.escape_ascii()
                    );
                    prop_assert!(visible(&value));
                    // Taken as bytes: the segments are the value's own.
                    let rebuilt: Vec<u8> = segments
                        .iter()
                        .flat_map(|s| [&b"/"[..], s].concat())
                        .collect();
                    prop_assert!(
                        value[ROOT.len()..].starts_with(&rebuilt),
                        "{}",
                        value.escape_ascii()
                    );
                }
                Located::NotFound => {}
            }
        }
    }

    #[test]
    fn accel_open_is_safe(value in value()) {
        if let Ok(located) = accel_path(PREFIX, &value) {
            check_segments(&located)?;
            match located {
                Located::Open { .. } => {
                    prop_assert!(value.starts_with(PREFIX));
                    prop_assert!(visible(&value));
                }
                Located::NotFound => {}
            }
        }
    }

    /// Arbitrary roots, prefixes and values never panic, and every `Open`
    /// is safe whatever the root or prefix.
    #[test]
    fn arbitrary_input_never_panics(
        base in proptest::collection::vec(any::<u8>(), 0..8),
        value in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
        if let Ok(located) = sendfile_path(&base, &value) {
            check_segments(&located)?;
        }
        if let Ok(located) = accel_path(&base, &value) {
            check_segments(&located)?;
        }
    }
}
