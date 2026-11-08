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
        (ConfigErrorKind::OnFileChangeOnProxy, 7, 5)
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
