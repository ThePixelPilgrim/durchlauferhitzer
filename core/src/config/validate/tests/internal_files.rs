//! Tests for the `internal_files` route setting (FR-CFG-26).

use super::super::super::model::{FileHeader, InternalFiles};
use super::*;

/// A valid file whose only route proxies and has `line` after its body
/// limits (the setting is at 7:5).
fn with_setting(line: &str) -> String {
    in_host(&format!(
        "  path / {{\n    proxy 127.0.0.1:3000;\n    max_request_body 1;\n    \
         max_response_body 1;\n    {line}\n  }}"
    ))
}

fn internal_files(config: &Config) -> Option<InternalFiles> {
    let route = config.hosts.first()?.routes.first()?;
    match &route.action {
        RouteAction::Proxy(proxy) => proxy.internal_files.clone(),
        RouteAction::Static { .. } => panic!("expected a proxy route"),
    }
}

fn count_error() -> ConfigErrorKind {
    ConfigErrorKind::WrongValueCount("internal_files".to_string())
}

#[test]
fn x_sendfile_parses() {
    let config = ok(&with_setting("internal_files x-sendfile /srv/files;"));
    assert_eq!(
        internal_files(&config),
        Some(InternalFiles {
            header: FileHeader::XSendfile,
            root: PathBuf::from("/srv/files"),
            on_file_change: OnFileChange::Abort,
        })
    );
    assert_eq!(
        internal_files(&ok(&with_setting(""))),
        None,
        "it is optional"
    );
}

#[test]
fn x_accel_redirect_parses() {
    for prefix in ["/protected/", "/", "/a/b/"] {
        let config = ok(&with_setting(&format!(
            "internal_files x-accel-redirect {prefix} /srv/files;"
        )));
        assert_eq!(
            internal_files(&config),
            Some(InternalFiles {
                header: FileHeader::XAccelRedirect {
                    prefix: prefix.to_string()
                },
                root: PathBuf::from("/srv/files"),
                on_file_change: OnFileChange::Abort,
            }),
            "{prefix}"
        );
    }
}

#[test]
fn on_file_change_with_internal_files() {
    let config = ok(&with_setting(
        "internal_files x-sendfile /srv/files;\n    on_file_change allow_growth;",
    ));
    assert_eq!(
        internal_files(&config).map(|f| f.on_file_change),
        Some(OnFileChange::AllowGrowth)
    );
    // Written before `internal_files` it counts the same.
    let config = ok(&with_setting(
        "on_file_change allow_growth;\n    internal_files x-sendfile /srv/files;",
    ));
    assert_eq!(
        internal_files(&config).map(|f| f.on_file_change),
        Some(OnFileChange::AllowGrowth)
    );
    let error = load(&with_setting("on_file_change abort;")).unwrap_err();
    assert_eq!(
        (error.kind.clone(), error.pos.line, error.pos.column),
        (ConfigErrorKind::OnFileChangeWithoutFiles, 7, 5)
    );
    assert_eq!(
        error.to_string(),
        "7:5: `on_file_change` is only allowed on static routes and proxy routes \
         with `internal_files`"
    );
}

#[test]
fn internal_files_on_static_is_an_error() {
    let text = in_host("  path / {\n    static /s;\n    internal_files x-sendfile /f;\n  }");
    assert_eq!(err(&text), (ConfigErrorKind::InternalFilesOnStatic, 5, 5));
    assert_eq!(
        load(&text).unwrap_err().to_string(),
        "5:5: `internal_files` is only allowed on proxy routes"
    );
}

#[test]
fn internal_files_twice_is_duplicate() {
    assert_eq!(
        err(&with_setting(
            "internal_files x-sendfile /a;\n    internal_files x-sendfile /b;"
        )),
        (
            ConfigErrorKind::DuplicateKey("internal_files".to_string()),
            8,
            5
        )
    );
}

#[test]
fn wrong_counts() {
    for bad in [
        "internal_files;",
        "internal_files x-sendfile;",
        "internal_files x-sendfile /a /b;",
        "internal_files x-accel-redirect;",
        "internal_files x-accel-redirect /srv;",
        "internal_files x-accel-redirect /p/ /a /b;",
    ] {
        assert_eq!(err(&with_setting(bad)), (count_error(), 7, 5), "{bad}");
    }
}

/// A valid file with a static route on `static_root` and, after it, a
/// proxy route with `internal_files <form>` (the setting is at 10:5).
fn beside_static(static_root: &str, form: &str) -> String {
    in_host(&format!(
        "  path / {{\n    static {static_root};\n  }}\n  path /app/ {{\n    \
         proxy 127.0.0.1:3000;\n    max_request_body 1;\n    max_response_body 1;\n    \
         internal_files {form};\n  }}"
    ))
}

fn under_static(internal: &str, static_root: &str) -> ConfigErrorKind {
    ConfigErrorKind::InternalFilesUnderStatic {
        internal_root: internal.to_string(),
        static_root: static_root.to_string(),
    }
}

/// Owner decision 2026-11-09 (FR-CFG-26): an `internal_files` root equal
/// to or below a static root is an error (its files would be public
/// through the static route), compared component by component; a root
/// above a static root, or beside one, is fine.
#[test]
fn internal_root_at_or_below_a_static_root_is_an_error() {
    for (static_root, internal) in [
        ("/srv/www", "/srv/www"),
        ("/srv/www", "/srv/www/private"),
        ("/srv/www", "/srv/www/a/b"),
        ("/", "/srv/files"),
    ] {
        for form in [
            format!("x-sendfile {internal}"),
            format!("x-accel-redirect /p/ {internal}"),
        ] {
            assert_eq!(
                err(&beside_static(static_root, &form)),
                (under_static(internal, static_root), 10, 5),
                "{static_root} {form}"
            );
        }
    }
    assert_eq!(
        load(&beside_static("/srv/www", "x-sendfile /srv/www/private"))
            .unwrap_err()
            .to_string(),
        "10:5: `internal_files` root `/srv/www/private` is equal to or below the static \
         root `/srv/www`, which would make its files public"
    );
}

#[test]
fn internal_root_above_or_beside_a_static_root_is_fine() {
    for (static_root, internal) in [
        ("/srv/www", "/srv"),
        ("/srv/www/public", "/srv/www"),
        ("/srv/www", "/srv/www-x"),
        ("/srv/www-x", "/srv/www"),
        ("/srv/www", "/srv/files"),
    ] {
        let config = ok(&beside_static(
            static_root,
            &format!("x-sendfile {internal}"),
        ));
        assert_eq!(config.hosts.len(), 1, "{static_root} {internal}");
    }
}

/// Every static root of the file counts, in any host and written before
/// or after the `internal_files` route.
#[test]
fn every_static_root_of_the_file_counts() {
    let proxy = "  path / {\n    proxy 127.0.0.1:3000;\n    max_request_body 1;\n    \
                 max_response_body 1;\n    internal_files x-sendfile /srv/www/private;\n  }";
    let later = cfg(&format!(
        "host a {{\n{proxy}\n}}\nhost b {{\n  path / {{ static /srv/www; }}\n}}\n"
    ));
    assert_eq!(
        err(&later),
        (under_static("/srv/www/private", "/srv/www"), 7, 5)
    );
    let earlier = cfg(&format!(
        "host b {{\n  path / {{ static /srv/www; }}\n}}\nhost a {{\n{proxy}\n}}\n"
    ));
    assert_eq!(
        err(&earlier),
        (under_static("/srv/www/private", "/srv/www"), 10, 5)
    );
}

#[test]
fn bad_values() {
    let cases = [
        ("nginx /srv", "nginx"),
        ("x-sendfile srv", "srv"),
        ("x-sendfile /", "/"),
        ("x-sendfile /srv/", "/srv/"),
        ("x-sendfile /a/../b", "/a/../b"),
        ("x-sendfile \"/my files\"", "/my files"),
        ("x-sendfile \"/caf\u{e9}\"", "/caf\u{e9}"),
        ("x-sendfile \"/a\tb\"", "/a\tb"),
        ("x-accel-redirect /p/ srv", "srv"),
        ("x-accel-redirect /p/ /", "/"),
        ("x-accel-redirect /p/ /srv/", "/srv/"),
        ("x-accel-redirect /p/ /a/../b", "/a/../b"),
        ("x-accel-redirect /p/ \"/my files\"", "/my files"),
        ("x-accel-redirect /protected /srv", "/protected"),
        ("x-accel-redirect protected/ /srv", "protected/"),
        ("x-accel-redirect \"/a?b/\" /srv", "/a?b/"),
        ("x-accel-redirect /a//b/ /srv", "/a//b/"),
        ("x-accel-redirect /%61/ /srv", "/%61/"),
        ("x-accel-redirect /./ /srv", "/./"),
        // `?` makes every X-Sendfile value a 502; Rack splits
        // X-Accel-Mapping on `,` and then on the first `=`.
        ("x-sendfile \"/srv/a?b\"", "/srv/a?b"),
        ("x-sendfile \"/srv/a=b\"", "/srv/a=b"),
        ("x-sendfile \"/srv/a,b\"", "/srv/a,b"),
        ("x-accel-redirect /p/ \"/srv/a?b\"", "/srv/a?b"),
        ("x-accel-redirect /p/ \"/srv/a=b\"", "/srv/a=b"),
        ("x-accel-redirect /p/ \"/srv/a,b\"", "/srv/a,b"),
        ("x-accel-redirect \"/a,b/\" /srv", "/a,b/"),
    ];
    for (setting, value) in cases {
        assert_eq!(
            err(&with_setting(&format!("internal_files {setting};"))),
            (invalid("internal_files", value), 7, 5),
            "{setting}"
        );
    }
}
