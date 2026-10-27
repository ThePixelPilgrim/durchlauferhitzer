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
  disk_threads 8;
  shutdown_timeout 5s;
  memory_limit 64MiB;
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
            disk_threads: 8,
            shutdown_timeout: Duration::from_secs(5),
            memory_limit: 64 << 20,
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
            disk_threads: 4,
            shutdown_timeout: Duration::from_secs(30),
            memory_limit: 1 << 30,
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
        "disk_threads 0;",
        "shutdown_timeout 0s;",
        "memory_limit 0;",
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
    // 1024 connections of 1 MiB + 1 KiB need more than the default 1 GiB.
    let config = ok(&with_limit("max_head 1MiB;\n  memory_limit 2GiB;"));
    assert_eq!(config.limits.max_head, 1 << 20);
    assert_eq!(
        err(&with_limit("max_head 1048577;\n  memory_limit 2GiB;")),
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
            LimitKey::DiskThreads => 11,
            LimitKey::ShutdownTimeout => 12,
            LimitKey::MemoryLimit => 13,
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

/// Lines that keep the memory budget out of the way of a cap test: a roomy
/// `memory_limit`, or (when that is the key under test) one connection.
fn extra_for(key: &str) -> &'static str {
    if key == "memory_limit" {
        "max_connections 1;\n  max_connections_per_ip 1;\n  "
    } else {
        "memory_limit 1024GiB;\n  "
    }
}

/// FR-CFG-21 (owner decision 2026-10-27): the lowest and highest accepted
/// value of each capped key, and one past each, rejected at line:column.
#[test]
fn per_key_caps() {
    // (key, accepted values, rejected values); extra lines make the
    // accepted file consistent with the other rules.
    let cases: [(&str, &[&str], &[&str]); 7] = [
        ("max_connections", &["64", "65536"], &["0", "65537"]),
        ("max_headers", &["1", "1000"], &["0", "1001"]),
        (
            "header_timeout",
            &["1ms", "1440m", "86400s"],
            &["0ms", "86400001ms", "1441m", "86401s"],
        ),
        ("shutdown_timeout", &["1ms", "1440m"], &["0s", "1441m"]),
        ("disk_threads", &["1", "64"], &["0", "65"]),
        (
            "memory_limit",
            &["1MiB", "1024GiB"],
            &["1048575", "1025GiB"],
        ),
        ("body_idle_timeout", &["1ms", "1440m"], &["0ms", "1441m"]),
    ];
    for (key, good, bad) in cases {
        for value in good {
            let extra = extra_for(key);
            ok(&with_limit(&format!("{extra}{key} {value};")));
        }
        for value in bad {
            let extra = extra_for(key);
            let line = 3 + extra.matches('\n').count() as u32;
            assert_eq!(
                err(&with_limit(&format!("{extra}{key} {value};"))),
                (invalid(key, value), line, 3),
                "{key} {value}"
            );
        }
    }
}

#[test]
fn per_ip_may_not_exceed_max_connections() {
    ok(&with_limit(
        "max_connections 10;\n  max_connections_per_ip 10;",
    ));
    // Reported at the per-IP setting.
    assert_eq!(
        err(&with_limit(
            "max_connections 10;\n  max_connections_per_ip 11;"
        )),
        (invalid("max_connections_per_ip", "11"), 4, 3)
    );
    assert_eq!(
        err(&with_limit(
            "max_connections_per_ip 11;\n  max_connections 10;"
        )),
        (invalid("max_connections_per_ip", "11"), 3, 3)
    );
    // The default per-IP limit (64) applies when only max_connections is set.
    assert_eq!(
        err(&with_limit("max_connections 10;")),
        (invalid("max_connections", "10"), 3, 3)
    );
}

/// NFR-RES-01 by hand, for the defaults: receive max(16384, 8192 + 2) +
/// send 1024; proxy adds upstream receive max(16384, 16384) and the head
/// buffer 16384 + 100 + 128.
#[test]
fn worst_case_per_connection_for_defaults() {
    let limits = Limits::default();
    assert_eq!(worst_case_per_connection(&limits, false), 16384 + 1024);
    assert_eq!(
        worst_case_per_connection(&limits, true),
        16384 + 1024 + 16384 + (16384 + 100 + 128)
    );
    // A large max_head dominates every term; a tiny one is floored.
    let big = Limits {
        max_head: 1 << 20,
        ..Limits::default()
    };
    assert_eq!(
        worst_case_per_connection(&big, true),
        (1 << 20) + 1024 + (1 << 20) + ((1 << 20) + 100 + 128)
    );
    let tiny = Limits {
        max_request_line: 100,
        max_head: 100,
        ..Limits::default()
    };
    assert_eq!(worst_case_per_connection(&tiny, false), 102 + 1024);
    assert_eq!(
        worst_case_per_connection(&tiny, true),
        102 + 1024 + 16384 + (100 + 100 + 128)
    );
}

#[test]
fn memory_budget_is_enforced_at_the_limits_block() {
    let text = cfg(
        "limits {\n  max_connections 65536;\n  max_connections_per_ip 1;\n  max_head 1MiB;\n}\nhost a { path / { static /s; } }\n",
    );
    let needed = 65536u64 * (1_048_576 + 1024);
    assert_eq!(
        err(&text),
        (
            ConfigErrorKind::MemoryBudget {
                needed,
                limit: 1 << 30
            },
            2,
            1
        )
    );
    let message = match load(&text) {
        Err(e) => e.to_string(),
        Ok(_) => String::new(),
    };
    assert!(message.starts_with("2:1: "), "{message}");
    assert!(
        message.contains(&needed.to_string()) && message.contains("1073741824"),
        "{message}"
    );
    // Exactly at the limit passes; one byte less fails.
    let at = format!(
        "limits {{\n  max_connections 65536;\n  max_connections_per_ip 1;\n  max_head 1MiB;\n  memory_limit {needed};\n}}\nhost a {{ path / {{ static /s; }} }}\n"
    );
    ok(&cfg(&at));
    let under = at.replace(&needed.to_string(), &(needed - 1).to_string());
    assert_eq!(
        err(&cfg(&under)),
        (
            ConfigErrorKind::MemoryBudget {
                needed,
                limit: needed - 1
            },
            2,
            1
        )
    );
}

/// The proxy terms only count when a proxy route exists.
#[test]
fn memory_budget_counts_proxy_terms_only_with_a_proxy_route() {
    let limits = "limits {\n  max_connections 1000;\n  max_connections_per_ip 1;\n  memory_limit 20MiB;\n}\n";
    // 1000 * 17408 = 17.4 MB fits in 20 MiB; with proxy 1000 * 50404 does not.
    ok(&cfg(&format!(
        "{limits}host a {{ path / {{ static /s; }} }}\n"
    )));
    let proxied = format!(
        "{limits}host a {{ path / {{ static /s; }} path /p {{ proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 1MiB; }} }}\n"
    );
    assert_eq!(
        err(&cfg(&proxied)),
        (
            ConfigErrorKind::MemoryBudget {
                needed: 1000 * 50404,
                limit: 20 << 20
            },
            2,
            1
        )
    );
}

#[test]
fn memory_budget_without_a_limits_block_is_checked_at_the_first_host() {
    // Defaults always fit (1024 * 50404 < 1 GiB); this pins that it loads.
    let config = ok(&cfg(
        "\nhost a { path /p { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 1MiB; } }\n",
    ));
    assert_eq!(config.limits, Limits::default());
}
