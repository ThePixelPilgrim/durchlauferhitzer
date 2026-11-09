//! Validation of one `path` block into a [`Route`] (FR-CFG-13, -15, -26).

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::http::target::validate_path;

use super::super::lexer::Pos;
use super::super::model::{
    BodyLimit, ConfigError, ConfigErrorKind, FileHeader, InternalFiles, OnFileChange, Pooling,
    ProxyRoute, Route, RouteAction, StripPrefix,
};
use super::super::syntax::{Block, Item, Setting};
use super::{
    duplicate_key, fail, invalid_value, no_value, one_value, parse_backend, parse_body_limit,
    path_segments, unknown_key, valid_root, wrong_value_count,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteKey {
    Static,
    Proxy,
    StripPrefix,
    MaxRequestBody,
    MaxResponseBody,
    OnFileChange,
    Pool,
    InternalFiles,
}

impl RouteKey {
    /// Every route key with its name: the one list the parser and the
    /// example config's test read.
    pub(super) const ALL: [(&'static str, RouteKey); 8] = [
        ("static", Self::Static),
        ("proxy", Self::Proxy),
        ("strip_prefix", Self::StripPrefix),
        ("max_request_body", Self::MaxRequestBody),
        ("max_response_body", Self::MaxResponseBody),
        ("on_file_change", Self::OnFileChange),
        ("pool", Self::Pool),
        ("internal_files", Self::InternalFiles),
    ];

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, key)| *key)
    }
}

/// An `internal_files` root: FR-CFG-15, and visible ASCII only (bytes
/// 0x21 to 0x7E), so that no `X-Sendfile` value can equal another root and
/// the root can go into `X-Accel-Mapping` (FR-CFG-26). `/` is refused: it
/// would make the whole filesystem readable. `?` is refused (every
/// `X-Sendfile` value would be a bad path), and so are `=` and `,` (Rack
/// splits `X-Accel-Mapping` on `,` and then on the first `=`).
fn valid_internal_root(root: &str) -> bool {
    root != "/"
        && valid_root(root)
        && root
            .bytes()
            .all(|b| (0x21..=0x7E).contains(&b) && !matches!(b, b'?' | b'=' | b','))
}

/// An `internal_files` prefix: starts and ends with `/` and is a valid path
/// (FR-CFG-13), without `,` (Rack splits `X-Accel-Mapping` on it). `/`
/// alone is fine.
fn valid_prefix(prefix: &str) -> bool {
    prefix.starts_with('/')
        && prefix.ends_with('/')
        && !prefix.contains(',')
        && validate_path(prefix).is_ok()
}

/// `x-sendfile <root>` or `x-accel-redirect <prefix> <root>`; the
/// `on_file_change` is filled in by the caller.
fn parse_internal_files(setting: &Setting) -> Result<InternalFiles, ConfigError> {
    let bad = |value: &str| invalid_value(&setting.key, value, setting.pos);
    let (header, root) = match setting.values.as_slice() {
        [] => return wrong_value_count(setting),
        [form, rest @ ..] => match (form.as_str(), rest) {
            ("x-sendfile", [root]) => (FileHeader::XSendfile, root),
            ("x-accel-redirect", [prefix, root]) => {
                if !valid_prefix(prefix) {
                    return Err(bad(prefix));
                }
                let prefix = prefix.clone();
                (FileHeader::XAccelRedirect { prefix }, root)
            }
            ("x-sendfile" | "x-accel-redirect", _) => return wrong_value_count(setting),
            _ => return Err(bad(form)),
        },
    };
    if !valid_internal_root(root) {
        return Err(bad(root));
    }
    Ok(InternalFiles {
        header,
        root: PathBuf::from(root),
        on_file_change: OnFileChange::Abort,
    })
}

/// A validated setting together with where it was written.
struct Slot<T> {
    pos: Pos,
    value: T,
}

/// Parses one `path` block; also returns where its `internal_files`
/// setting was written, if it has one, for the checks that need the whole
/// file (FR-CFG-26).
pub(super) fn parse_route(block: &Block) -> Result<(Route, Option<Pos>), ConfigError> {
    let segments = match block.args.as_slice() {
        [arg] if validate_path(arg).is_ok() => path_segments(arg),
        [arg] => return Err(invalid_value("path", arg, block.pos)),
        [] | [_, _, ..] => {
            return fail(
                ConfigErrorKind::WrongValueCount("path".to_string()),
                block.pos,
            );
        }
    };
    let mut static_root: Option<Slot<PathBuf>> = None;
    let mut backend: Option<Slot<SocketAddr>> = None;
    let mut strip: Option<Slot<StripPrefix>> = None;
    let mut max_request: Option<Slot<BodyLimit>> = None;
    let mut max_response: Option<Slot<BodyLimit>> = None;
    let mut on_change: Option<Slot<OnFileChange>> = None;
    let mut pool: Option<Slot<Pooling>> = None;
    let mut internal: Option<Slot<InternalFiles>> = None;
    for item in &block.items {
        let setting = match item {
            Item::Setting(setting) => setting,
            Item::Block(inner) => return unknown_key(&inner.name, inner.pos),
        };
        let name = setting.key.as_str();
        let pos = setting.pos;
        let Some(key) = RouteKey::from_name(name) else {
            return unknown_key(name, pos);
        };
        let taken = match key {
            RouteKey::Static => static_root.is_some(),
            RouteKey::Proxy => backend.is_some(),
            RouteKey::StripPrefix => strip.is_some(),
            RouteKey::MaxRequestBody => max_request.is_some(),
            RouteKey::MaxResponseBody => max_response.is_some(),
            RouteKey::OnFileChange => on_change.is_some(),
            RouteKey::Pool => pool.is_some(),
            RouteKey::InternalFiles => internal.is_some(),
        };
        if taken {
            return duplicate_key(name, pos);
        }
        let bad = |value: &str| invalid_value(name, value, pos);
        match key {
            RouteKey::Static => {
                let value = one_value(setting)?;
                if !valid_root(value) {
                    return Err(bad(value));
                }
                static_root = Some(Slot {
                    pos,
                    value: PathBuf::from(value),
                });
            }
            RouteKey::Proxy => {
                let value = one_value(setting)?;
                backend = Some(Slot {
                    pos,
                    value: parse_backend(value).ok_or_else(|| bad(value))?,
                });
            }
            RouteKey::StripPrefix => {
                no_value(setting)?;
                strip = Some(Slot {
                    pos,
                    value: StripPrefix::Strip,
                });
            }
            RouteKey::MaxRequestBody => {
                let value = one_value(setting)?;
                max_request = Some(Slot {
                    pos,
                    value: parse_body_limit(value).ok_or_else(|| bad(value))?,
                });
            }
            RouteKey::MaxResponseBody => {
                let value = one_value(setting)?;
                max_response = Some(Slot {
                    pos,
                    value: parse_body_limit(value).ok_or_else(|| bad(value))?,
                });
            }
            RouteKey::OnFileChange => {
                let value = one_value(setting)?;
                let parsed = match value {
                    "abort" => OnFileChange::Abort,
                    "allow_growth" => OnFileChange::AllowGrowth,
                    _ => return Err(bad(value)),
                };
                on_change = Some(Slot { pos, value: parsed });
            }
            RouteKey::Pool => {
                let value = one_value(setting)?;
                let parsed = match value {
                    "on" => Pooling::On,
                    "off" => Pooling::Off,
                    _ => return Err(bad(value)),
                };
                pool = Some(Slot { pos, value: parsed });
            }
            RouteKey::InternalFiles => {
                internal = Some(Slot {
                    pos,
                    value: parse_internal_files(setting)?,
                });
            }
        }
    }
    // A static route with `internal_files` fails below.
    let internal_pos = internal.as_ref().map(|slot| slot.pos);
    let action = match (static_root, backend) {
        (Some(root), None) => {
            if let Some(slot) = &strip {
                return fail(ConfigErrorKind::StripPrefixOnStatic, slot.pos);
            }
            if let Some(slot) = max_request.as_ref().or(max_response.as_ref()) {
                return fail(ConfigErrorKind::BodyLimitOnStatic, slot.pos);
            }
            if let Some(slot) = &pool {
                return fail(ConfigErrorKind::PoolOnStatic, slot.pos);
            }
            if let Some(slot) = &internal {
                return fail(ConfigErrorKind::InternalFilesOnStatic, slot.pos);
            }
            RouteAction::Static {
                root: root.value,
                on_file_change: on_change.map_or(OnFileChange::Abort, |slot| slot.value),
            }
        }
        (None, Some(backend)) => {
            let internal_files = match (internal, on_change) {
                (Some(slot), change) => Some(InternalFiles {
                    on_file_change: change.map_or(OnFileChange::Abort, |slot| slot.value),
                    ..slot.value
                }),
                (None, Some(slot)) => return fail(ConfigErrorKind::OnFileChangeOnProxy, slot.pos),
                (None, None) => None,
            };
            let Some(max_request_body) = max_request else {
                return fail(
                    ConfigErrorKind::MissingBodyLimit("max_request_body"),
                    block.pos,
                );
            };
            let Some(max_response_body) = max_response else {
                return fail(
                    ConfigErrorKind::MissingBodyLimit("max_response_body"),
                    block.pos,
                );
            };
            RouteAction::Proxy(ProxyRoute {
                backend: backend.value,
                strip_prefix: strip.map_or(StripPrefix::Keep, |slot| slot.value),
                max_request_body: max_request_body.value,
                max_response_body: max_response_body.value,
                pool: pool.map_or(Pooling::On, |slot| slot.value),
                internal_files,
            })
        }
        (None, None) | (Some(_), Some(_)) => {
            return fail(ConfigErrorKind::RouteNeedsOneAction, block.pos);
        }
    };
    Ok((Route { segments, action }, internal_pos))
}
