//! Tests for `host` and `path` blocks.

use super::*;

#[test]
fn host_names_are_lowercased() {
    let config = ok(&cfg(
        "host Example.COM 10.0.0.1 [::A] { path / { static /s; } }\n",
    ));
    assert_eq!(
        config.hosts.first().map(|h| h.names.clone()),
        Some(strings(&["example.com", "10.0.0.1", "[::a]"]))
    );
}

#[test]
fn invalid_host_names() {
    for bad in ["a_b", ".a", "a.", "a..b", "[zz]", "[::1", "a:80", "a/b", ""] {
        assert_eq!(
            err(&cfg(&format!(
                "host \"{bad}\" {{\n  path / {{ static /s; }}\n}}\n"
            ))),
            (invalid("host", bad), 2, 1),
            "{bad:?}"
        );
    }
}

#[test]
fn host_needs_a_name() {
    assert_eq!(
        err(&cfg("host {\n}\n")),
        (ConfigErrorKind::WrongValueCount("host".to_string()), 2, 1)
    );
}

#[test]
fn host_only_allows_path_blocks() {
    assert_eq!(
        err(&in_host("  static /x;")),
        (ConfigErrorKind::UnknownKey("static".to_string()), 3, 3)
    );
    assert_eq!(
        err(&in_host("  limits { }")),
        (ConfigErrorKind::UnknownKey("limits".to_string()), 3, 3)
    );
}

#[test]
fn host_needs_a_path() {
    assert_eq!(
        err(&cfg("host a {\n}\n")),
        (ConfigErrorKind::MissingKey("path"), 2, 1)
    );
}

#[test]
fn duplicate_host_names() {
    assert_eq!(
        err(&cfg(
            "host a { path / { static /s; } }\nhost A { path / { static /s; } }\n"
        )),
        (ConfigErrorKind::DuplicateHost("a".to_string()), 3, 1)
    );
    assert_eq!(
        err(&cfg("host a a { path / { static /s; } }\n")),
        (ConfigErrorKind::DuplicateHost("a".to_string()), 2, 1)
    );
}

#[test]
fn path_takes_exactly_one_arg() {
    assert_eq!(
        err(&in_host("  path { static /s; }")),
        (ConfigErrorKind::WrongValueCount("path".to_string()), 3, 3)
    );
    assert_eq!(
        err(&in_host("  path / /x { static /s; }")),
        (ConfigErrorKind::WrongValueCount("path".to_string()), 3, 3)
    );
}

#[test]
fn path_must_start_with_slash() {
    assert_eq!(
        err(&in_host("  path x { static /s; }")),
        (invalid("path", "x"), 3, 3)
    );
}

#[test]
fn path_rejects_hostile_targets() {
    for bad in [
        "/a/../", "/a//b", "/%2e", "/a b", "/\u{e4}", "/a?b", "/p/..;/x", "/a%0a", "/%61pi",
    ] {
        assert_eq!(
            err(&in_host(&format!("  path \"{bad}\" {{ static /s; }}"))),
            (invalid("path", bad), 3, 3),
            "{bad}"
        );
    }
    ok(&in_host("  path /api/ { static /s; }"));
}

#[test]
fn duplicate_paths_in_one_host() {
    assert_eq!(
        err(&in_host(
            "  path /x { static /s; }\n  path /x { static /s; }"
        )),
        (ConfigErrorKind::DuplicatePath("/x".to_string()), 4, 3)
    );
    // The same path in different hosts is fine.
    let config = ok(&cfg(
        "host a { path /x { static /s; } }\nhost b { path /x { static /s; } }\n",
    ));
    assert_eq!(config.hosts.len(), 2);
}

#[test]
fn route_rejects_unknown_items() {
    assert_eq!(
        err(&in_host("  path / {\n    serve /s;\n  }")),
        (ConfigErrorKind::UnknownKey("serve".to_string()), 4, 5)
    );
    assert_eq!(
        err(&in_host("  path / {\n    static { }\n  }")),
        (ConfigErrorKind::UnknownKey("static".to_string()), 4, 5)
    );
}

#[test]
fn route_rejects_duplicate_keys() {
    assert_eq!(
        err(&in_host("  path / {\n    static /a;\n    static /b;\n  }")),
        (ConfigErrorKind::DuplicateKey("static".to_string()), 5, 5)
    );
}

#[test]
fn route_rejects_duplicate_proxy_and_option_keys() {
    for key in [
        "proxy",
        "strip_prefix",
        "max_request_body",
        "max_response_body",
    ] {
        let value = match key {
            "proxy" => "127.0.0.1:1",
            "strip_prefix" => "",
            _ => "1",
        };
        assert_eq!(
            err(&in_host(&format!(
                "  path / {{\n    {key} {value};\n    {key} {value};\n  }}"
            ))),
            (ConfigErrorKind::DuplicateKey(key.to_string()), 5, 5),
            "{key}"
        );
    }
}

#[test]
fn empty_strip_prefix_is_rejected() {
    assert_eq!(
        err(&proxy_path(
            "    strip_prefix \"\";\n    max_request_body 1;\n    max_response_body 1;"
        )),
        (
            ConfigErrorKind::WrongValueCount("strip_prefix".to_string()),
            5,
            5
        )
    );
}

#[test]
fn route_needs_exactly_one_action() {
    assert_eq!(
        err(&in_host("  path / { }")),
        (ConfigErrorKind::RouteNeedsOneAction, 3, 3)
    );
    assert_eq!(
        err(&in_host(
            "  path / { static /s; proxy 127.0.0.1:1; max_request_body 1; max_response_body 1; }"
        )),
        (ConfigErrorKind::RouteNeedsOneAction, 3, 3)
    );
}

#[test]
fn route_values_must_be_single() {
    assert_eq!(
        err(&in_host("  path / { static a b; }")),
        (
            ConfigErrorKind::WrongValueCount("static".to_string()),
            3,
            12
        )
    );
}

#[test]
fn static_root_rules() {
    for bad in ["relative", "/a/../b", "/a/./b", "/a//b", "/a/", "", "/.."] {
        assert_eq!(
            err(&in_host(&format!("  path / {{ static \"{bad}\"; }}"))),
            (invalid("static", bad), 3, 12),
            "{bad:?}"
        );
    }
    for good in ["/", "/srv/www", "/a.b/.hidden/c..d"] {
        let config = ok(&in_host(&format!("  path / {{ static {good}; }}")));
        assert_eq!(
            config.hosts.first().and_then(|h| h.routes.first().cloned()),
            Some(static_route(&[], good))
        );
    }
}

fn proxy_route(host: &str, port: u16) -> RouteAction {
    RouteAction::Proxy(ProxyRoute {
        backend: BackendAddr {
            host: host.to_string(),
            port,
        },
        strip_prefix: StripPrefix::Keep,
        max_request_body: BodyLimit::Bytes(1),
        max_response_body: BodyLimit::Bytes(1),
    })
}

fn proxy_cfg(proxy: &str) -> String {
    in_host(&format!(
        "  path / {{\n    proxy \"{proxy}\";\n    max_request_body 1;\n    max_response_body 1;\n  }}"
    ))
}

#[test]
fn proxy_rules() {
    let bad = [
        "localhost",
        "localhost:0",
        "localhost:65536",
        "localhost:",
        ":80",
        "a_b:80",
        "[::1]",
        "::1:80",
        "[zz]:80",
        "a:b",
        "a:+80",
        "a:80:80",
    ];
    for b in bad {
        assert_eq!(err(&proxy_cfg(b)), (invalid("proxy", b), 4, 5), "{b:?}");
    }
    let good = [
        ("backend.local:3000", "backend.local", 3000),
        ("Backend.Local:3000", "backend.local", 3000),
        ("127.0.0.1:1", "127.0.0.1", 1),
        ("[::1]:3000", "::1", 3000),
        ("localhost:65535", "localhost", 65535),
    ];
    for (input, host, port) in good {
        let config = ok(&proxy_cfg(input));
        assert_eq!(
            config
                .hosts
                .first()
                .and_then(|h| h.routes.first())
                .map(|r| r.action.clone()),
            Some(proxy_route(host, port)),
            "{input:?}"
        );
    }
}

fn proxy_path(extra: &str) -> String {
    in_host(&format!(
        "  path /api/ {{\n    proxy 127.0.0.1:3000;\n{extra}\n  }}"
    ))
}

/// The first route of the first host.
fn first_route(config: &Config) -> Option<Route> {
    config.hosts.first().and_then(|h| h.routes.first().cloned())
}

fn first_proxy(config: &Config) -> ProxyRoute {
    match first_route(config).map(|r| r.action) {
        Some(RouteAction::Proxy(proxy)) => proxy,
        other => panic!("expected a proxy route, got {other:?}"),
    }
}

#[test]
fn strip_prefix_on_static_is_rejected() {
    assert_eq!(
        err(&in_host(
            "  path / {\n    static /s;\n    strip_prefix;\n  }"
        )),
        (ConfigErrorKind::StripPrefixOnStatic, 5, 5)
    );
}

#[test]
fn strip_prefix_is_a_flag() {
    let rest = "    max_request_body 1;\n    max_response_body 1;";
    let config = ok(&proxy_path(&format!("    strip_prefix;\n{rest}")));
    assert_eq!(first_proxy(&config).strip_prefix, StripPrefix::Strip);
    let config = ok(&proxy_path(rest));
    assert_eq!(first_proxy(&config).strip_prefix, StripPrefix::Keep);
    assert_eq!(
        err(&proxy_path(&format!("    strip_prefix /api;\n{rest}"))),
        (
            ConfigErrorKind::WrongValueCount("strip_prefix".to_string()),
            5,
            5
        )
    );
}

/// Values that were once accepted or rejected as prefixes of the path: all
/// of them are now a wrong value count, since `strip_prefix` is a flag.
#[test]
fn strip_prefix_rejects_any_value() {
    let rest = "    max_request_body 1;\n    max_response_body 1;";
    for strip in [
        "/app", "/api/x", "/api", "/ap", "/api/", "/", "api", "/api /x",
    ] {
        assert_eq!(
            err(&proxy_path(&format!("    strip_prefix {strip};\n{rest}"))),
            (
                ConfigErrorKind::WrongValueCount("strip_prefix".to_string()),
                5,
                5
            ),
            "strip_prefix {strip}"
        );
    }
}

/// `strip_prefix;` strips the route's own segments, so it always ends at a
/// segment boundary; it is accepted on every route, including `/`.
#[test]
fn strip_prefix_strips_whole_route_segments() {
    let rest = "    max_request_body 1;\n    max_response_body 1;";
    for (path, segments) in [
        ("/", &[][..]),
        ("/api", &["api"][..]),
        ("/api/", &["api"][..]),
        ("/a/b/c/", &["a", "b", "c"][..]),
    ] {
        let config = ok(&in_host(&format!(
            "  path {path} {{\n    proxy 127.0.0.1:3000;\n    strip_prefix;\n{rest}\n  }}"
        )));
        let route = first_route(&config).map(|r| r.segments);
        assert_eq!(route, Some(strings(segments)), "{path}");
        assert_eq!(first_proxy(&config).strip_prefix, StripPrefix::Strip);
    }
}

#[test]
fn paths_with_equal_segments_are_duplicates() {
    assert_eq!(
        err(&in_host(
            "  path /api { static /s; }\n  path /api/ { static /s; }"
        )),
        (ConfigErrorKind::DuplicatePath("/api/".to_string()), 4, 3)
    );
    assert_eq!(
        err(&in_host(
            "  path /a/b/ { static /s; }\n  path /a/b { static /s; }"
        )),
        (ConfigErrorKind::DuplicatePath("/a/b".to_string()), 4, 3)
    );
    // A shared leading segment is not a duplicate.
    ok(&in_host(
        "  path /api { static /s; }\n  path /api/v1 { static /s; }",
    ));
}

#[test]
fn segments_are_stored() {
    for (path, segments) in [
        ("/", &[][..]),
        ("/api", &["api"][..]),
        ("/api/v1", &["api", "v1"][..]),
        ("/api/v1/", &["api", "v1"][..]),
    ] {
        let config = ok(&in_host(&format!("  path {path} {{ static /s; }}")));
        assert_eq!(
            first_route(&config),
            Some(static_route(segments, "/s")),
            "{path}"
        );
    }
}

#[test]
fn body_limits_on_static_are_rejected() {
    assert_eq!(
        err(&in_host(
            "  path / {\n    static /s;\n    max_request_body 1;\n  }"
        )),
        (ConfigErrorKind::BodyLimitOnStatic, 5, 5)
    );
    assert_eq!(
        err(&in_host(
            "  path / {\n    static /s;\n    max_response_body unlimited;\n  }"
        )),
        (ConfigErrorKind::BodyLimitOnStatic, 5, 5)
    );
}

#[test]
fn proxy_route_without_max_request_body_is_rejected() {
    assert_eq!(
        err(&proxy_path("    max_response_body 1;")),
        (ConfigErrorKind::MissingBodyLimit("max_request_body"), 3, 3)
    );
}

#[test]
fn proxy_route_without_max_response_body_is_rejected() {
    assert_eq!(
        err(&proxy_path("    max_request_body 1;")),
        (ConfigErrorKind::MissingBodyLimit("max_response_body"), 3, 3)
    );
    assert_eq!(
        err(&proxy_path("")),
        (ConfigErrorKind::MissingBodyLimit("max_request_body"), 3, 3)
    );
}

#[test]
fn body_limit_values() {
    assert_eq!(
        err(&proxy_path(
            "    max_request_body 10MB;\n    max_response_body 1;"
        )),
        (invalid("max_request_body", "10MB"), 5, 5)
    );
    assert_eq!(
        err(&proxy_path(
            "    max_request_body 1;\n    max_response_body Unlimited;"
        )),
        (invalid("max_response_body", "Unlimited"), 6, 5)
    );
    assert_eq!(
        err(&proxy_path(
            "    max_request_body 1 2;\n    max_response_body 1;"
        )),
        (
            ConfigErrorKind::WrongValueCount("max_request_body".to_string()),
            5,
            5
        )
    );
    ok(&proxy_path(
        "    max_request_body 0;\n    max_response_body unlimited;",
    ));
}

fn first_on_file_change(config: &Config) -> OnFileChange {
    match first_route(config).map(|r| r.action) {
        Some(RouteAction::Static { on_file_change, .. }) => on_file_change,
        other => panic!("expected a static route, got {other:?}"),
    }
}

#[test]
fn on_file_change_values() {
    let static_path = |extra: &str| in_host(&format!("  path / {{\n    static /s;\n{extra}\n  }}"));
    assert_eq!(
        first_on_file_change(&ok(&static_path(""))),
        OnFileChange::Abort
    );
    assert_eq!(
        first_on_file_change(&ok(&static_path("    on_file_change abort;"))),
        OnFileChange::Abort
    );
    assert_eq!(
        first_on_file_change(&ok(&static_path("    on_file_change allow_growth;"))),
        OnFileChange::AllowGrowth
    );
    assert_eq!(
        err(&static_path("    on_file_change ignore;")),
        (invalid("on_file_change", "ignore"), 5, 5)
    );
    for bad in ["on_file_change;", "on_file_change abort allow_growth;"] {
        assert_eq!(
            err(&static_path(&format!("    {bad}"))),
            (
                ConfigErrorKind::WrongValueCount("on_file_change".to_string()),
                5,
                5
            ),
            "{bad}"
        );
    }
    assert_eq!(
        err(&static_path(
            "    on_file_change abort;\n    on_file_change abort;"
        )),
        (
            ConfigErrorKind::DuplicateKey("on_file_change".to_string()),
            6,
            5
        )
    );
}

#[test]
fn on_file_change_on_proxy_is_rejected() {
    assert_eq!(
        err(&proxy_path(
            "    max_request_body 1;\n    max_response_body 1;\n    on_file_change abort;"
        )),
        (ConfigErrorKind::OnFileChangeOnProxy, 7, 5)
    );
}
