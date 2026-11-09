use super::*;
use crate::file::FileStamp;

/// The 1000-byte file every table row asks for: mtime 2023-11-14 22:13:20
/// UTC plus 5 ns.
const STAMP: FileStamp = FileStamp {
    size: 1000,
    modified_ns: 1_700_000_000_000_000_005,
};
const TAG: &str = "\"3e8-17979cfe362a0005\"";
const LAST_MODIFIED: &str = "Tue, 14 Nov 2023 22:13:20 GMT";

fn headers(lines: &[(&str, &str)]) -> Vec<Header> {
    lines
        .iter()
        .map(|(name, value)| Header {
            name: (*name).to_owned(),
            value: value.as_bytes().to_vec(),
        })
        .collect()
}

fn judge(method: &Method, lines: &[(&str, &str)], stamp: FileStamp) -> Ranged {
    RangeRequest::of(&headers(lines)).judge(method, &Validators::of(stamp), stamp.size)
}

fn get(lines: &[(&str, &str)]) -> Ranged {
    judge(&Method::Get, lines, STAMP)
}

#[test]
fn the_fixture_is_the_file_it_names() {
    let validators = Validators::of(STAMP);
    assert_eq!(validators.etag.as_bytes(), TAG.as_bytes());
    assert_eq!(&validators.last_modified[..], LAST_MODIFIED.as_bytes());
}

/// FR-STATIC-19 and the planner calls: one valid `bytes` range of a
/// `GET` gets its part, clamped to the file; one that does not overlap
/// is unsatisfiable; several ranges, a malformed one, another unit,
/// several `Range` lines and every method but `GET` get the whole file.
#[test]
fn judges_ranges() {
    let part = |first, last| Ranged::Part { first, last };
    let rows: [(&str, Ranged); 15] = [
        ("bytes=0-99", part(0, 99)),
        ("bytes=990-", part(990, 999)),
        ("bytes=-10", part(990, 999)),
        ("bytes=-2000", part(0, 999)),
        ("bytes=500-5000", part(500, 999)),
        ("BYTES=0-0", part(0, 0)),
        ("bytes= 0-1 ,", part(0, 1)),
        ("bytes=1000-", Ranged::Unsatisfiable),
        ("bytes=-0", Ranged::Unsatisfiable),
        ("bytes=0-1,5-6", Ranged::Whole),
        ("bytes=5-1", Ranged::Whole),
        ("items=0-1", Ranged::Whole),
        ("bytes=0x-1", Ranged::Whole),
        ("bytes=18446744073709551616-", Ranged::Whole),
        ("bytes=,", Ranged::Whole),
    ];
    for (value, want) in rows {
        assert_eq!(get(&[("range", value)]), want, "{value}");
    }
    for value in [
        "bytes = 0-1",
        "bytes=0 -1",
        "bytes=0- 1",
        "bytes=",
        "bytes=-",
        "bytes=+1-2",
        "=0-1",
        "bytes0-1",
        "bytes=1-2-3",
    ] {
        assert_eq!(get(&[("range", value)]), Ranged::Whole, "{value}");
    }
    assert_eq!(
        get(&[("range", "bytes=0-1"), ("range", "bytes=0-1")]),
        Ranged::Whole,
        "two Range lines"
    );
    let range = [("range", "bytes=0-1")];
    assert_eq!(judge(&Method::Head, &range, STAMP), Ranged::Whole);
    let post = Method::Other("POST".to_owned());
    assert_eq!(judge(&post, &range, STAMP), Ranged::Whole);
    assert_eq!(get(&[]), Ranged::Whole, "no Range");
    assert_eq!(
        RangeRequest::NONE.judge(&Method::Get, &Validators::of(STAMP), 1000),
        Ranged::Whole
    );
}

/// Owner decision 2026-11-09 (FR-STATIC-19): an empty file's `Range` is
/// ignored, so the whole (empty) file goes out with `200`, whatever the
/// range and with or without a matching `If-Range`.
#[test]
fn empty_file_ranges_are_ignored() {
    let empty = FileStamp { size: 0, ..STAMP };
    let tag = format!("\"0-{:x}\"", STAMP.modified_ns);
    assert_eq!(Validators::of(empty).etag.as_bytes(), tag.as_bytes());
    for value in ["bytes=0-", "bytes=-5", "bytes=-0", "bytes=0-0", "bytes=5-9"] {
        for if_range in [None, Some(tag.as_str())] {
            let mut lines = vec![("range", value)];
            lines.extend(if_range.map(|tag| ("if-range", tag)));
            assert_eq!(
                judge(&Method::Get, &lines, empty),
                Ranged::Whole,
                "{value} {if_range:?}"
            );
        }
    }
}

/// `If-Range` keeps the range only for the file's own strong `ETag` or
/// exactly its `Last-Modified` bytes, on one line; anything else sends the
/// whole file, also for an unsatisfiable range. Without `Range` it is
/// ignored.
#[test]
fn if_range_keeps_or_drops_the_range() {
    let range = ("range", "bytes=0-99");
    let kept = Ranged::Part { first: 0, last: 99 };
    let weak = format!("W/{TAG}");
    let later = "Tue, 14 Nov 2023 22:13:21 GMT";
    let rfc850 = "Tuesday, 14-Nov-23 22:13:20 GMT";
    let other_file = "\"3e9-17979cfe362a0005\"";
    let rows: [(&[&str], Ranged); 9] = [
        (&[TAG], kept),
        (&[weak.as_str()], Ranged::Whole),
        (&["\"x\""], Ranged::Whole),
        (&[other_file], Ranged::Whole),
        (&[LAST_MODIFIED], kept),
        (&[later], Ranged::Whole),
        (&[rfc850], Ranged::Whole),
        (&[TAG, TAG], Ranged::Whole),
        (&[&format!("{TAG} x")], Ranged::Whole),
    ];
    for (values, want) in rows {
        let mut lines = vec![range];
        lines.extend(values.iter().map(|v| ("if-range", *v)));
        assert_eq!(get(&lines), want, "{values:?}");
    }
    // A failed If-Range sends the whole file even for a range that would
    // be unsatisfiable.
    assert_eq!(
        get(&[("range", "bytes=5000-"), ("if-range", "\"x\"")]),
        Ranged::Whole
    );
    assert_eq!(
        get(&[("range", "bytes=5000-"), ("if-range", TAG)]),
        Ranged::Unsatisfiable
    );
    assert_eq!(get(&[("if-range", TAG)]), Ranged::Whole, "no Range");
}

/// The request's range is fixed-size state, copied into the static
/// request (NFR-RES-01): no allocation whatever the head holds.
#[test]
fn a_range_request_is_small_and_copy() {
    fn is_copy<T: Copy>() {}
    is_copy::<RangeRequest>();
    assert!(std::mem::size_of::<RangeRequest>() <= 64);
}
