//! The head buffer's room for Rack's hints (NFR-RES-01, FR-PROXY-32), by
//! their documented sizes, computed from the config independently of the
//! core.

use durchlauferhitzer_core::config::{Config, FileHeader, RouteAction};

/// The longest hint lines any `internal_files` route of `config` adds to
/// a request head: `x-sendfile-type: X-Sendfile\r\n` (29 bytes), or
/// `x-sendfile-type: X-Accel-Redirect\r\n` (35) and
/// `x-accel-mapping: <root>/=<prefix>\r\n` (21 + root + prefix); 0
/// without `internal_files`.
pub(crate) fn hint_room(config: &Config) -> usize {
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
