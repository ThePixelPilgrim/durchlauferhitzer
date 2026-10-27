//! Tests for the `limits` block.

use super::*;

#[test]
fn loads_all_limits() {
    let config = ok(&cfg("\
limits {
  max_connections 7;
  max_connections_per_ip 3;
  max_request_line 1KiB;
  max_head 2KiB;
  max_headers 20;
  header_timeout 500ms;
  keepalive_timeout 2m;
  send_idle_timeout 5s;
  backend_connect_timeout 2s;
  backend_response_timeout 90s;
  body_idle_timeout 1500ms;
}
host a { path / { static /s; } }
"));
    assert_eq!(
        config.limits,
        Limits {
            max_connections: 7,
            max_connections_per_ip: 3,
            max_request_line: 1024,
            max_head: 2048,
            max_headers: 20,
            header_timeout: Duration::from_millis(500),
            keepalive_timeout: Duration::from_secs(120),
            send_idle_timeout: Duration::from_secs(5),
            backend_connect_timeout: Duration::from_secs(2),
            backend_response_timeout: Duration::from_secs(90),
            body_idle_timeout: Duration::from_millis(1500),
        }
    );
}

/// FR-CFG-20: the defaults are the spec's table.
#[test]
fn default_limits_are_the_spec_table() {
    assert_eq!(
        Limits::default(),
        Limits {
            max_connections: 1024,
            max_connections_per_ip: 64,
            max_request_line: 8 * 1024,
            max_head: 16 * 1024,
            max_headers: 100,
            header_timeout: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(60),
            send_idle_timeout: Duration::from_secs(30),
            backend_connect_timeout: Duration::from_secs(5),
            backend_response_timeout: Duration::from_secs(60),
            body_idle_timeout: Duration::from_secs(30),
        }
    );
}

#[test]
fn limits_must_be_positive_and_valid() {
    for line in [
        "max_connections 0;",
        "max_connections 4294967296;",
        "max_connections -1;",
        "max_connections 1KiB;",
        "max_connections_per_ip 0;",
        "max_headers 0;",
        "max_request_line 0;",
        "max_request_line 1kb;",
        "max_head 0;",
        "header_timeout 0s;",
        "keepalive_timeout 0ms;",
        "send_idle_timeout 5;",
        "backend_connect_timeout 0s;",
        "backend_connect_timeout 5;",
        "backend_response_timeout 0ms;",
        "backend_response_timeout 1KiB;",
        "body_idle_timeout 0m;",
        "body_idle_timeout 18446744073709551616s;",
    ] {
        let mut words = line.trim_end_matches(';').split(' ');
        let key = words.next().unwrap_or("");
        let value = words.next().unwrap_or("");
        assert_eq!(
            err(&with_limit(line)),
            (invalid(key, value), 3, 3),
            "{line}"
        );
    }
}

#[test]
fn max_request_line_must_not_exceed_max_head() {
    // Default max_head is 16KiB.
    assert_eq!(
        err(&with_limit("max_request_line 20KiB;")),
        (invalid("max_request_line", "20KiB"), 3, 3)
    );
    // Default max_request_line is 8KiB.
    assert_eq!(
        err(&with_limit("max_head 4KiB;")),
        (invalid("max_head", "4KiB"), 3, 3)
    );
    // Both set: reported at max_request_line.
    assert_eq!(
        err(&with_limit("max_request_line 8KiB;\n  max_head 4KiB;")),
        (invalid("max_request_line", "8KiB"), 3, 3)
    );
    ok(&with_limit("max_request_line 4KiB;\n  max_head 4KiB;"));
}

#[test]
fn limits_block_structure() {
    assert_eq!(
        err(&with_limit("bogus 1;")),
        (ConfigErrorKind::UnknownKey("bogus".to_string()), 3, 3)
    );
    assert_eq!(
        err(&with_limit("max_head { }")),
        (ConfigErrorKind::UnknownKey("max_head".to_string()), 3, 3)
    );
    assert_eq!(
        err(&with_limit("max_connections 1;\n  max_connections 2;")),
        (
            ConfigErrorKind::DuplicateKey("max_connections".to_string()),
            4,
            3
        )
    );
    assert_eq!(
        err(&with_limit("max_connections 1 2;")),
        (
            ConfigErrorKind::WrongValueCount("max_connections".to_string()),
            3,
            3
        )
    );
    assert_eq!(
        err(&cfg("limits x {\n}\nhost a { path / { static /s; } }\n")),
        (ConfigErrorKind::WrongValueCount("limits".to_string()), 2, 1)
    );
}

#[test]
fn max_head_upper_bound() {
    let config = ok(&with_limit("max_head 1MiB;"));
    assert_eq!(config.limits.max_head, 1 << 20);
    assert_eq!(
        err(&with_limit("max_head 1048577;")),
        (invalid("max_head", "1048577"), 3, 3)
    );
}

/// `LimitKey::ALL` is the one list of `limits` keys: every variant is in
/// it exactly once (the match below breaks when a variant is added), and
/// each name parses back to its key.
#[test]
fn limit_key_list_is_complete() {
    fn index(key: LimitKey) -> usize {
        match key {
            LimitKey::MaxConnections => 0,
            LimitKey::MaxConnectionsPerIp => 1,
            LimitKey::MaxRequestLine => 2,
            LimitKey::MaxHead => 3,
            LimitKey::MaxHeaders => 4,
            LimitKey::HeaderTimeout => 5,
            LimitKey::KeepaliveTimeout => 6,
            LimitKey::SendIdleTimeout => 7,
            LimitKey::BackendConnectTimeout => 8,
            LimitKey::BackendResponseTimeout => 9,
            LimitKey::BodyIdleTimeout => 10,
        }
    }
    let mut seen = [false; LimitKey::ALL.len()];
    for (name, key) in LimitKey::ALL {
        assert_eq!(LimitKey::from_name(name), Some(key), "{name}");
        let slot = seen.get_mut(index(key));
        assert_eq!(slot.as_deref(), Some(&false), "{name} listed twice");
        if let Some(slot) = slot {
            *slot = true;
        }
    }
    assert!(seen.iter().all(|s| *s), "a limits key is missing from ALL");
}
