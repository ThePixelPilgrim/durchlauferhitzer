//! The head buffer's room for Rack's hints and a file's merged head
//! (NFR-RES-01, FR-PROXY-32), by their documented sizes, computed from
//! the config independently of the core.

use durchlauferhitzer_core::config::{Config, FileHeader, RouteAction};

/// What the head buffer holds beyond `max_head + max_headers + 128` for
/// `config`: the hints' room and a file's merged head's.
pub(crate) fn head_room(config: &Config) -> usize {
    hint_room(config) + file_head_room(config)
}

/// The longest hint lines any `internal_files` route of `config` adds to
/// a request head: `x-sendfile-type: X-Sendfile\r\n` (29 bytes), or
/// `x-sendfile-type: X-Accel-Redirect\r\n` (35) and
/// `x-accel-mapping: <root>/=<prefix>\r\n` (21 + root + prefix); 0
/// without `internal_files`.
fn hint_room(config: &Config) -> usize {
    config
        .hosts
        .iter()
        .flat_map(|h| &h.routes)
        .filter_map(|r| match &r.action {
            RouteAction::Proxy(p) => p.internal_files.as_ref(),
            RouteAction::Static { .. } => None,
        })
        .map(|f| match &f.header {
            FileHeader::XSendfile => 29,
            FileHeader::XAccelRedirect { prefix } => {
                35 + 21 + f.root.as_os_str().len() + prefix.len()
            }
        })
        .max()
        .unwrap_or(0)
}

/// The room a backend-directed file's merged head needs beyond the
/// backend's kept lines (FR-PROXY-32, NFR-RES-01) on a config with any
/// `internal_files` route, by the documented sizes of the core's lines:
/// `HTTP/1.1 206 Partial Content` (30 with CRLF), `Date` (37),
/// `Content-Type` with `text/javascript; charset=utf-8` (46),
/// `Content-Length` (18 + 20 digits), `Content-Range: bytes a-b/size`
/// (25 + 3 × 20 digits), `ETag` (8 + 35), `Last-Modified` (17 + 29),
/// `Accept-Ranges: bytes` (22), `X-Content-Type-Options: nosniff` (33),
/// `Connection: close` (19) and the blank line (2): 401 bytes; 0 without
/// `internal_files`.
fn file_head_room(config: &Config) -> usize {
    let any = config
        .hosts
        .iter()
        .flat_map(|h| &h.routes)
        .any(|r| match &r.action {
            RouteAction::Proxy(p) => p.internal_files.is_some(),
            RouteAction::Static { .. } => false,
        });
    if any {
        30 + 37 + 46 + (18 + 20) + (25 + 3 * 20) + (8 + 35) + (17 + 29) + 22 + 33 + 19 + 2
    } else {
        0
    }
}
