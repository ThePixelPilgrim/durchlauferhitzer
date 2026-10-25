//! Tests for configuration validation: top level, listen, user.

use super::super::model::{
    BackendAddr, BodyLimit, Limits, ProxyRoute, Route, RouteAction, StripPrefix, VirtualHost,
};
use super::super::syntax::SyntaxErrorKind;
use super::*;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

mod limits;
mod routes;

const SPEC_EXAMPLE: &str = "\
listen 127.0.0.1:8080;
user www-data;            # drop privileges to this user after binding

limits {
  max_connections 1024;
  header_timeout 10s;
}

host example.com www.example.com {
  path / { static /srv/www; }
  path /api/ {
    proxy 127.0.0.1:3000;
    strip_prefix;
    max_request_body 10MiB;
    max_response_body unlimited;
  }
}
";

fn err(text: &str) -> (ConfigErrorKind, u32, u32) {
    match load(text) {
        Err(e) => (e.kind, e.pos.line, e.pos.column),
        Ok(config) => panic!("expected error, got {config:?}"),
    }
}

fn ok(text: &str) -> Config {
    match load(text) {
        Ok(config) => config,
        Err(e) => panic!("expected success, got {e}"),
    }
}

/// A valid file with `body` appended after the `listen` line (line 1).
fn cfg(body: &str) -> String {
    format!("listen 127.0.0.1:80;\n{body}")
}

/// A valid file whose first host block (line 2) contains `inner`.
fn in_host(inner: &str) -> String {
    cfg(&format!("host a {{\n{inner}\n}}\n"))
}

/// A valid file with `line` as the first setting of a `limits` block
/// (the setting is at 3:3).
fn with_limit(line: &str) -> String {
    cfg(&format!(
        "limits {{\n  {line}\n}}\nhost a {{ path / {{ static /s; }} }}\n"
    ))
}

fn invalid(key: &str, value: &str) -> ConfigErrorKind {
    ConfigErrorKind::InvalidValue {
        key: key.to_string(),
        value: value.to_string(),
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn static_route(segments: &[&str], root: &str) -> Route {
    Route {
        segments: strings(segments),
        action: RouteAction::Static {
            root: PathBuf::from(root),
        },
    }
}

#[test]
fn loads_spec_example() {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let expected = Config {
        listen: vec![addr],
        user: Some("www-data".to_string()),
        limits: Limits {
            max_connections: 1024,
            header_timeout: Duration::from_secs(10),
            ..Limits::default()
        },
        hosts: vec![VirtualHost {
            names: strings(&["example.com", "www.example.com"]),
            routes: vec![
                static_route(&[], "/srv/www"),
                Route {
                    segments: strings(&["api"]),
                    action: RouteAction::Proxy(ProxyRoute {
                        backend: BackendAddr {
                            host: "127.0.0.1".to_string(),
                            port: 3000,
                        },
                        strip_prefix: StripPrefix::Strip,
                        max_request_body: BodyLimit::Bytes(10 << 20),
                        max_response_body: BodyLimit::Unlimited,
                    }),
                },
            ],
        }],
    };
    assert_eq!(ok(SPEC_EXAMPLE), expected);
}

#[test]
fn minimal_config_uses_default_limits() {
    let config = ok(&cfg("host a { path / { static /s; } }\n"));
    assert_eq!(config.limits, Limits::default());
    assert_eq!(config.user, None);
    assert_eq!(config.listen.len(), 1);
}

#[test]
fn loads_several_listen_addresses() {
    let config = ok("listen 127.0.0.1:80 [::1]:8080;\nhost a { path / { static /s; } }");
    let a: SocketAddr = "127.0.0.1:80".parse().unwrap();
    let b: SocketAddr = "[::1]:8080".parse().unwrap();
    assert_eq!(config.listen, vec![a, b]);
}

#[test]
fn wraps_syntax_errors() {
    assert_eq!(
        err("listen 127.0.0.1:80;\n}"),
        (
            ConfigErrorKind::Syntax(SyntaxErrorKind::UnexpectedToken),
            2,
            1
        )
    );
}

#[test]
fn unknown_top_level_key() {
    assert_eq!(
        err(&cfg("lsiten x;\n")),
        (ConfigErrorKind::UnknownKey("lsiten".to_string()), 2, 1)
    );
}

#[test]
fn wrong_form_is_unknown_key() {
    assert_eq!(
        err(&cfg("limits 5;\n")),
        (ConfigErrorKind::UnknownKey("limits".to_string()), 2, 1)
    );
    assert_eq!(
        err(&cfg("host a b;\n")),
        (ConfigErrorKind::UnknownKey("host".to_string()), 2, 1)
    );
    assert_eq!(
        err("listen { }\n"),
        (ConfigErrorKind::UnknownKey("listen".to_string()), 1, 1)
    );
    assert_eq!(
        err(&cfg("user { }\n")),
        (ConfigErrorKind::UnknownKey("user".to_string()), 2, 1)
    );
}

#[test]
fn duplicate_top_level_keys() {
    assert_eq!(
        err("listen 127.0.0.1:80;\nlisten 127.0.0.1:81;\n"),
        (ConfigErrorKind::DuplicateKey("listen".to_string()), 2, 1)
    );
    assert_eq!(
        err(&cfg("user a;\nuser b;\n")),
        (ConfigErrorKind::DuplicateKey("user".to_string()), 3, 1)
    );
    assert_eq!(
        err(&cfg("limits { }\nlimits { }\n")),
        (ConfigErrorKind::DuplicateKey("limits".to_string()), 3, 1)
    );
}

#[test]
fn missing_listen_and_host() {
    assert_eq!(
        err("host a { path / { static /s; } }"),
        (ConfigErrorKind::MissingKey("listen"), 1, 1)
    );
    assert_eq!(
        err("listen 127.0.0.1:80;"),
        (ConfigErrorKind::MissingKey("host"), 1, 1)
    );
    assert_eq!(err(""), (ConfigErrorKind::MissingKey("listen"), 1, 1));
}

#[test]
fn invalid_listen_address() {
    assert_eq!(
        err("listen 127.0.0.1:80 localhost:80;"),
        (invalid("listen", "localhost:80"), 1, 1)
    );
    assert_eq!(
        err("listen 127.0.0.1;"),
        (invalid("listen", "127.0.0.1"), 1, 1)
    );
}

#[test]
fn user_must_be_one_valid_name() {
    assert_eq!(
        err(&cfg("user a b;\n")),
        (ConfigErrorKind::WrongValueCount("user".to_string()), 2, 1)
    );
    let too_long = "a".repeat(33);
    for bad in ["Root", "1abc", "", "-x", "a b", "a.b", too_long.as_str()] {
        assert_eq!(
            err(&cfg(&format!("user \"{bad}\";\n"))),
            (invalid("user", bad), 2, 1),
            "{bad:?}"
        );
    }
    let longest = "a".repeat(32);
    for good in ["_svc-1", "www-data", "a", longest.as_str()] {
        let config = ok(&cfg(&format!(
            "user {good};\nhost a {{ path / {{ static /s; }} }}\n"
        )));
        assert_eq!(config.user.as_deref(), Some(good));
    }
}

#[test]
fn missing_value_is_rejected() {
    let count = |key: &str| ConfigErrorKind::WrongValueCount(key.to_string());
    assert_eq!(
        err("listen;\nhost a { path / { static /s; } }\n"),
        (count("listen"), 1, 1)
    );
    assert_eq!(err(&cfg("user;\n")), (count("user"), 2, 1));
    assert_eq!(
        err(&with_limit("max_connections;")),
        (count("max_connections"), 3, 3)
    );
    assert_eq!(
        err(&in_host("  path / { static; }")),
        (count("static"), 3, 12)
    );
    assert_eq!(
        err(&in_host("  path / { proxy; }")),
        (count("proxy"), 3, 12)
    );
    assert_eq!(
        err(&in_host(
            "  path / {\n    proxy 127.0.0.1:1;\n    max_request_body;\n    max_response_body 1;\n  }"
        )),
        (count("max_request_body"), 5, 5)
    );
    assert_eq!(
        err(&in_host(
            "  path / {\n    proxy 127.0.0.1:1;\n    max_request_body 1;\n    max_response_body;\n  }"
        )),
        (count("max_response_body"), 6, 5)
    );
}

#[test]
fn listen_rejects_duplicates_and_port_zero() {
    let host = "\nhost a { path / { static /s; } }\n";
    assert_eq!(
        err(&format!("listen 127.0.0.1:80 [::1]:80 127.0.0.1:80;{host}")),
        (invalid("listen", "127.0.0.1:80"), 1, 1)
    );
    assert_eq!(
        err(&format!("listen 127.0.0.1:0;{host}")),
        (invalid("listen", "127.0.0.1:0"), 1, 1)
    );
    assert_eq!(
        err(&format!("listen 127.0.0.1:80 [::1]:0;{host}")),
        (invalid("listen", "[::1]:0"), 1, 1)
    );
    // The same port on different addresses is fine.
    let config = ok(&format!("listen 127.0.0.1:80 [::1]:80;{host}"));
    assert_eq!(config.listen.len(), 2);
}

#[test]
fn display_names_line_and_column() {
    let e = load("listen 127.0.0.1:80;\nhost a { path / { static /s; } }\n    lsiten x;")
        .err()
        .map(|e| e.to_string());
    assert_eq!(e.as_deref(), Some("3:5: unknown key `lsiten`"));
}

/// The shipped example config must always load: it is documentation users
/// copy from, so it may never drift from what the parser accepts.
#[test]
fn example_config_loads() {
    let text = include_str!("../../../../../examples/durchlauferhitzer.conf");
    let config = load(text).map_err(|e| e.to_string());
    assert!(config.is_ok(), "examples/durchlauferhitzer.conf: {config:?}");
}
