//! Validation of one `path` block into a [`Route`] (FR-CFG-13, -15, -26).

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::http::target::validate_path;

use super::super::lexer::Pos;
use super::super::model::{
    BodyLimit, ConfigError, ConfigErrorKind, OnFileChange, Pooling, ProxyRoute, Route, RouteAction,
    StripPrefix,
};
use super::super::syntax::{Block, Item};
use super::{
    duplicate_key, fail, invalid_value, no_value, one_value, parse_backend, parse_body_limit,
    path_segments, unknown_key, valid_root,
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
}

impl RouteKey {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "static" => Some(Self::Static),
            "proxy" => Some(Self::Proxy),
            "strip_prefix" => Some(Self::StripPrefix),
            "max_request_body" => Some(Self::MaxRequestBody),
            "max_response_body" => Some(Self::MaxResponseBody),
            "on_file_change" => Some(Self::OnFileChange),
            "pool" => Some(Self::Pool),
            _ => None,
        }
    }
}

/// A validated setting together with where it was written.
struct Slot<T> {
    pos: Pos,
    value: T,
}

pub(super) fn parse_route(block: &Block) -> Result<Route, ConfigError> {
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
        }
    }
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
            RouteAction::Static {
                root: root.value,
                on_file_change: on_change.map_or(OnFileChange::Abort, |slot| slot.value),
            }
        }
        (None, Some(backend)) => {
            if let Some(slot) = &on_change {
                return fail(ConfigErrorKind::OnFileChangeOnProxy, slot.pos);
            }
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
            })
        }
        (None, None) | (Some(_), Some(_)) => {
            return fail(ConfigErrorKind::RouteNeedsOneAction, block.pos);
        }
    };
    Ok(Route { segments, action })
}
