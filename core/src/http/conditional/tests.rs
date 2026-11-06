use super::*;

const STAMP: FileStamp = FileStamp {
    size: 0x2a,
    modified_ns: 1_700_000_000_123_456_789,
};

/// `STAMP`'s tag: 0x2a and 1_700_000_000_123_456_789 in hex.
const TAG: &str = "\"2a-17979cfe3d85cd15\"";

/// 2023-11-14 22:13:20 UTC, `STAMP`'s mtime in whole seconds.
const MTIME: u64 = 1_700_000_000;

fn headers(lines: &[(&str, &str)]) -> Vec<Header> {
    lines
        .iter()
        .map(|(name, value)| Header {
            name: (*name).to_owned(),
            value: value.as_bytes().to_vec(),
        })
        .collect()
}

fn inm(values: &[&str]) -> Conditional {
    let lines: Vec<(&str, &str)> = values.iter().map(|v| ("if-none-match", *v)).collect();
    Conditional::of(&headers(&lines))
}

fn ims(values: &[&str]) -> Conditional {
    let lines: Vec<(&str, &str)> = values.iter().map(|v| ("if-modified-since", *v)).collect();
    Conditional::of(&headers(&lines))
}

#[test]
fn the_etag_is_size_and_mtime_in_lowercase_hex() {
    assert_eq!(EntityTag::of(STAMP).as_bytes(), TAG.as_bytes());
    let zero = FileStamp {
        size: 0,
        modified_ns: 0,
    };
    assert_eq!(EntityTag::of(zero).as_bytes(), b"\"0-0\"");
    let max = FileStamp {
        size: u64::MAX,
        modified_ns: u64::MAX,
    };
    let tag = EntityTag::of(max);
    assert_eq!(tag.as_bytes(), b"\"ffffffffffffffff-ffffffffffffffff\"");
    assert_eq!(tag.as_bytes().len(), ETAG_MAX_LEN);
}

#[test]
fn last_modified_is_the_mtime_in_whole_seconds() {
    let v = Validators::of(STAMP);
    assert_eq!(&v.last_modified, b"Tue, 14 Nov 2023 22:13:20 GMT");
}

#[test]
fn no_conditional_headers_mean_modified() {
    let c = Conditional::of(&headers(&[("host", "a"), ("if-match", TAG)]));
    assert_eq!(c, Conditional::NONE);
    assert_eq!(c.not_modified(STAMP, Some(MTIME)), Freshness::Modified);
}

#[test]
fn a_matching_tag_strong_or_weak_is_not_modified() {
    let weak = format!("W/{TAG}");
    let listed = format!("\"x\", {TAG}");
    let spaced = format!(" , \"a,b\" ,,\t{TAG} ,");
    for value in [TAG, weak.as_str(), listed.as_str(), spaced.as_str()] {
        let c = inm(&[value]);
        assert_eq!(
            c.not_modified(STAMP, None),
            Freshness::NotModified,
            "{value}"
        );
    }
    // Several lines are one list.
    let c = inm(&["\"x\"", TAG]);
    assert_eq!(c.not_modified(STAMP, None), Freshness::NotModified);
}

#[test]
fn star_matches_any_file() {
    let c = inm(&["*"]);
    assert_eq!(c.if_none_match, IfNoneMatch::Any);
    assert_eq!(c.not_modified(STAMP, None), Freshness::NotModified);
}

#[test]
fn other_tags_are_modified() {
    for value in [
        "\"x\"",
        "\"2A-17979CFE3D85CD15\"",
        "\"02a-17979cfe3d85cd15\"",
        "\"2a-17979cfe3d85cd16\"",
        "\"2a\"",
        "",
        ",",
    ] {
        let c = inm(&[value]);
        assert!(matches!(c.if_none_match, IfNoneMatch::Tags(_)), "{value:?}");
        assert_eq!(
            c.not_modified(STAMP, None),
            Freshness::Modified,
            "{value:?}"
        );
    }
}

/// Not RFC 9110 section 13.1.2's grammar: the field is present but
/// matches nothing (section 13.1.3: the request "contains" it, so
/// `If-Modified-Since` is ignored; controller ruling 2026-11-06).
#[test]
fn a_malformed_if_none_match_matches_nothing() {
    let bare = TAG.trim_matches('"');
    let unclosed = format!("{TAG}, \"x");
    let joined = format!("{TAG}\"x\"");
    let lower_weak = format!("w/{TAG}");
    for values in [
        vec![bare],
        vec![unclosed.as_str()],
        vec![joined.as_str()],
        vec![lower_weak.as_str()],
        vec!["W/ \"x\""],
        vec!["\"a b\""],
        vec!["*, \"x\""],
        vec!["*", TAG],
        vec!["*", "*"],
        vec![TAG, "x"],
    ] {
        let c = inm(&values);
        assert_eq!(c.if_none_match, IfNoneMatch::Malformed, "{values:?}");
        assert_eq!(
            c.not_modified(STAMP, None),
            Freshness::Modified,
            "{values:?}"
        );
    }
}

#[test]
fn if_modified_since_compares_whole_seconds() {
    let now = Some(MTIME + 100);
    for (date, freshness) in [
        ("Tue, 14 Nov 2023 22:13:20 GMT", Freshness::NotModified),
        ("Tue, 14 Nov 2023 22:13:21 GMT", Freshness::NotModified),
        ("Tue, 14 Nov 2023 22:13:19 GMT", Freshness::Modified),
        ("Tuesday, 14-Nov-23 22:13:20 GMT", Freshness::NotModified),
        ("Tue Nov 14 22:13:20 2023", Freshness::NotModified),
        ("Tue Nov 14 22:13:19 2023", Freshness::Modified),
    ] {
        assert_eq!(ims(&[date]).not_modified(STAMP, now), freshness, "{date}");
    }
}

#[test]
fn a_bad_repeated_or_future_if_modified_since_is_ignored() {
    let now = Some(MTIME + 100);
    let date = "Tue, 14 Nov 2023 22:13:20 GMT";
    for values in [
        vec!["yesterday"],
        vec!["Tue, 14 Nov 2023 22:13:20 UTC"],
        vec![date, date],
    ] {
        let c = ims(&values);
        assert_eq!(c.if_modified_since, None, "{values:?}");
        assert_eq!(
            c.not_modified(STAMP, now),
            Freshness::Modified,
            "{values:?}"
        );
    }
    // Later than the server's time: invalid.
    let future = "Tue, 14 Nov 2023 22:15:01 GMT";
    assert_eq!(ims(&[future]).not_modified(STAMP, now), Freshness::Modified);
    // At the server's time: valid.
    let at_now = "Tue, 14 Nov 2023 22:15:00 GMT";
    assert_eq!(
        ims(&[at_now]).not_modified(STAMP, now),
        Freshness::NotModified
    );
    // No time yet: no date can be judged.
    assert_eq!(ims(&[date]).not_modified(STAMP, None), Freshness::Modified);
}

#[test]
fn if_none_match_decides_when_present() {
    let old = "Tue, 14 Nov 2023 22:13:19 GMT";
    let new = "Tue, 14 Nov 2023 22:13:20 GMT";
    let now = Some(MTIME + 100);
    let both = |tag: &str, date: &str| {
        Conditional::of(&headers(&[
            ("if-modified-since", date),
            ("if-none-match", tag),
        ]))
        .not_modified(STAMP, now)
    };
    assert_eq!(both(TAG, old), Freshness::NotModified);
    assert_eq!(both("\"x\"", new), Freshness::Modified);
    // A malformed If-None-Match is still present: the date is ignored.
    assert_eq!(both("x", new), Freshness::Modified);
    assert_eq!(both("x", old), Freshness::Modified);
    // So is an empty list.
    assert_eq!(both(",", new), Freshness::Modified);
}
