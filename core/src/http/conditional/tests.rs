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

/// The `n`th of a set of distinct stamps: no other stamp matches its tag.
fn nth_stamp(n: u64) -> FileStamp {
    FileStamp {
        size: 0x1000 + n,
        modified_ns: 7,
    }
}

fn nth_tag(n: u64) -> String {
    String::from_utf8_lossy(EntityTag::of(nth_stamp(n)).as_bytes()).into_owned()
}

/// Owner decision 2026-11-07: at most `MAX_TAGS` distinct tags are kept,
/// so a huge list costs no memory; a match among later ones is ignored.
#[test]
fn at_most_sixteen_distinct_tags_are_kept() {
    assert_eq!(MAX_TAGS, 16);
    let tags: Vec<String> = (0..3000).map(nth_tag).collect();
    let c = inm(&[tags.join(", ").as_str()]);
    let IfNoneMatch::Tags(kept) = &c.if_none_match else {
        panic!("{:?}", c.if_none_match);
    };
    assert!(kept.len() <= MAX_TAGS, "{}", kept.len());
    assert!(kept.capacity() <= MAX_TAGS, "{}", kept.capacity());
    for n in 0..16 {
        assert_eq!(
            c.not_modified(nth_stamp(n), None),
            Freshness::NotModified,
            "tag {n}"
        );
    }
    for n in [16, 17, 2999] {
        assert_eq!(
            c.not_modified(nth_stamp(n), None),
            Freshness::Modified,
            "tag {n}"
        );
    }
}

/// Tags of any form count, across all lines, and a repeated tag (strong
/// or weak alike: weak comparison) counts once.
#[test]
fn repeats_count_once_and_other_tags_count_too() {
    let mut list: Vec<String> = Vec::new();
    for n in 0..15 {
        list.push(nth_tag(n));
        list.push(format!("W/{}", nth_tag(n)));
    }
    list.push(nth_tag(0));
    let joined = list.join(",");
    // Then the 16th distinct tag, then the 17th.
    let sixteenth = nth_tag(15);
    let seventeenth = nth_tag(16);
    let c = inm(&[joined.as_str(), sixteenth.as_str(), seventeenth.as_str()]);
    assert_eq!(c.not_modified(nth_stamp(15), None), Freshness::NotModified);
    assert_eq!(c.not_modified(nth_stamp(16), None), Freshness::Modified);

    // 16 foreign tags fill the list: the server's own tag after them is
    // ignored.
    let foreign: Vec<String> = (0..16).map(|n| format!("\"x{n}\"")).collect();
    let all = foreign.join(", ");
    let c = inm(&[all.as_str(), sixteenth.as_str()]);
    assert_eq!(c.not_modified(nth_stamp(15), None), Freshness::Modified);
    let fifteen = foreign.get(..15).unwrap_or_default().join(", ");
    let c = inm(&[fifteen.as_str(), sixteenth.as_str()]);
    assert_eq!(c.not_modified(nth_stamp(15), None), Freshness::NotModified);
}

/// The whole field is still read against the grammar: a malformed
/// element after the 16th tag makes it malformed.
#[test]
fn the_grammar_covers_ignored_tags_too() {
    let mut tags: Vec<String> = (0..20).map(nth_tag).collect();
    tags.push("x".to_owned());
    let c = inm(&[tags.join(", ").as_str()]);
    assert_eq!(c.if_none_match, IfNoneMatch::Malformed);
}
