//! Validation of the syntax tree into the configuration model.

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use crate::http::target::validate_path;
use crate::routing::same_segments;

use super::lexer::Pos;
use super::model::{
    BackendAddr, BodyLimit, Config, ConfigError, ConfigErrorKind, Limits, OnFileChange, ProxyRoute,
    Route, RouteAction, StripPrefix, VirtualHost,
};
use super::syntax::{Block, Item, Setting, parse};
use super::units::{parse_duration, parse_size};

/// Upper bound for `max_head`: one connection's head buffer.
const MAX_HEAD_LIMIT: usize = 1 << 20;

fn fail<T>(kind: ConfigErrorKind, pos: Pos) -> Result<T, ConfigError> {
    Err(ConfigError { kind, pos })
}

fn unknown_key<T>(key: &str, pos: Pos) -> Result<T, ConfigError> {
    fail(ConfigErrorKind::UnknownKey(key.to_string()), pos)
}

fn duplicate_key<T>(key: &str, pos: Pos) -> Result<T, ConfigError> {
    fail(ConfigErrorKind::DuplicateKey(key.to_string()), pos)
}

fn invalid_value(key: &str, value: &str, pos: Pos) -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::InvalidValue {
            key: key.to_string(),
            value: value.to_string(),
        },
        pos,
    }
}

fn wrong_value_count<T>(setting: &Setting) -> Result<T, ConfigError> {
    fail(
        ConfigErrorKind::WrongValueCount(setting.key.clone()),
        setting.pos,
    )
}

/// The single value of a setting that takes exactly one.
fn one_value(setting: &Setting) -> Result<&str, ConfigError> {
    match setting.values.as_slice() {
        [value] => Ok(value),
        [] | [_, _, ..] => wrong_value_count(setting),
    }
}

/// Checks that a flag setting has no value.
fn no_value(setting: &Setting) -> Result<(), ConfigError> {
    if setting.values.is_empty() {
        Ok(())
    } else {
        wrong_value_count(setting)
    }
}

/// The segments of a validated path: `/` is `[]`, `/a/b` and `/a/b/` are
/// `["a", "b"]`.
fn path_segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// A host name (lowercased), IPv4 literal, or bracketed IPv6 literal.
fn host_name(arg: &str) -> Option<String> {
    if let Some(inner) = arg.strip_prefix('[') {
        inner.strip_suffix(']')?.parse::<Ipv6Addr>().ok()?;
        return Some(arg.to_ascii_lowercase());
    }
    if arg.is_empty() || arg.starts_with('.') || arg.ends_with('.') || arg.contains("..") {
        return None;
    }
    if !arg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return None;
    }
    Some(arg.to_ascii_lowercase())
}

/// `[a-z_][a-z0-9_-]{0,31}`
fn valid_user(name: &str) -> bool {
    match name.as_bytes().split_first() {
        Some((first, rest)) => {
            (first.is_ascii_lowercase() || *first == b'_')
                && rest.len() <= 31
                && rest.iter().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-'
                })
        }
        None => false,
    }
}

/// Absolute, no `.`, `..` or empty components, no trailing `/` except `/`.
fn valid_root(root: &str) -> bool {
    if root == "/" {
        return true;
    }
    match root.strip_prefix('/') {
        Some(rest) => rest
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        None => false,
    }
}

/// Splits at the last `:`; host name, IPv4 or bracketed IPv6 literal, and a
/// port in 1..=65535. IPv6 literals are stored without brackets.
fn parse_backend(value: &str) -> Option<BackendAddr> {
    let (host, port) = value.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    let name = host_name(host)?;
    let unbracketed = name
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map(str::to_string);
    Some(BackendAddr {
        host: unbracketed.unwrap_or(name),
        port,
    })
}

/// A plain decimal count that is not zero.
fn parse_count<T: FromStr + Default + PartialEq>(value: &str) -> Option<T> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|n| *n != T::default())
}

fn parse_body_limit(value: &str) -> Option<BodyLimit> {
    if value == "unlimited" {
        Some(BodyLimit::Unlimited)
    } else {
        parse_size(value).map(BodyLimit::Bytes)
    }
}

/// One or more socket addresses, none with port 0, none twice.
fn parse_listen(setting: &Setting) -> Result<Vec<SocketAddr>, ConfigError> {
    if setting.values.is_empty() {
        return wrong_value_count(setting);
    }
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for value in &setting.values {
        let bad = || invalid_value("listen", value, setting.pos);
        let addr: SocketAddr = value.parse().map_err(|_| bad())?;
        if addr.port() == 0 || addrs.contains(&addr) {
            return Err(bad());
        }
        addrs.push(addr);
    }
    Ok(addrs)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LimitKey {
    MaxConnections,
    MaxConnectionsPerIp,
    MaxRequestLine,
    MaxHead,
    MaxHeaders,
    HeaderTimeout,
    KeepaliveTimeout,
    SendIdleTimeout,
}

impl LimitKey {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "max_connections" => Some(Self::MaxConnections),
            "max_connections_per_ip" => Some(Self::MaxConnectionsPerIp),
            "max_request_line" => Some(Self::MaxRequestLine),
            "max_head" => Some(Self::MaxHead),
            "max_headers" => Some(Self::MaxHeaders),
            "header_timeout" => Some(Self::HeaderTimeout),
            "keepalive_timeout" => Some(Self::KeepaliveTimeout),
            "send_idle_timeout" => Some(Self::SendIdleTimeout),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RouteKey {
    Static,
    Proxy,
    StripPrefix,
    MaxRequestBody,
    MaxResponseBody,
    OnFileChange,
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
            _ => None,
        }
    }
}

fn parse_limits(block: &Block) -> Result<Limits, ConfigError> {
    if !block.args.is_empty() {
        return fail(
            ConfigErrorKind::WrongValueCount("limits".to_string()),
            block.pos,
        );
    }
    let mut limits = Limits::default();
    let mut seen: Vec<&str> = Vec::new();
    let mut request_line: Option<&Setting> = None;
    let mut head: Option<&Setting> = None;
    for item in &block.items {
        let setting = match item {
            Item::Setting(setting) => setting,
            Item::Block(inner) => return unknown_key(&inner.name, inner.pos),
        };
        let name = setting.key.as_str();
        let Some(key) = LimitKey::from_name(name) else {
            return unknown_key(name, setting.pos);
        };
        if seen.contains(&name) {
            return duplicate_key(name, setting.pos);
        }
        seen.push(name);
        let value = one_value(setting)?;
        let bad = || invalid_value(name, value, setting.pos);
        let size = || {
            parse_size(value)
                .filter(|n| *n != 0)
                .and_then(|n| usize::try_from(n).ok())
        };
        let duration = || parse_duration(value).filter(|d: &Duration| !d.is_zero());
        match key {
            LimitKey::MaxConnections => {
                limits.max_connections = parse_count(value).ok_or_else(bad)?;
            }
            LimitKey::MaxConnectionsPerIp => {
                limits.max_connections_per_ip = parse_count(value).ok_or_else(bad)?;
            }
            LimitKey::MaxRequestLine => {
                limits.max_request_line = size().ok_or_else(bad)?;
                request_line = Some(setting);
            }
            LimitKey::MaxHead => {
                limits.max_head = size().filter(|n| *n <= MAX_HEAD_LIMIT).ok_or_else(bad)?;
                head = Some(setting);
            }
            LimitKey::MaxHeaders => limits.max_headers = parse_count(value).ok_or_else(bad)?,
            LimitKey::HeaderTimeout => limits.header_timeout = duration().ok_or_else(bad)?,
            LimitKey::KeepaliveTimeout => {
                limits.keepalive_timeout = duration().ok_or_else(bad)?;
            }
            LimitKey::SendIdleTimeout => {
                limits.send_idle_timeout = duration().ok_or_else(bad)?;
            }
        }
    }
    if limits.max_request_line > limits.max_head {
        if let Some(setting) = request_line.or(head) {
            let value = setting.values.first().map_or("", String::as_str);
            return Err(invalid_value(&setting.key, value, setting.pos));
        }
    }
    Ok(limits)
}

/// A validated setting together with where it was written.
struct Slot<T> {
    pos: Pos,
    value: T,
}

fn parse_route(block: &Block) -> Result<Route, ConfigError> {
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
    let mut backend: Option<Slot<BackendAddr>> = None;
    let mut strip: Option<Slot<StripPrefix>> = None;
    let mut max_request: Option<Slot<BodyLimit>> = None;
    let mut max_response: Option<Slot<BodyLimit>> = None;
    let mut on_change: Option<Slot<OnFileChange>> = None;
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
            })
        }
        (None, None) | (Some(_), Some(_)) => {
            return fail(ConfigErrorKind::RouteNeedsOneAction, block.pos);
        }
    };
    Ok(Route { segments, action })
}

fn parse_host(block: &Block, seen: &mut Vec<String>) -> Result<VirtualHost, ConfigError> {
    if block.args.is_empty() {
        return fail(
            ConfigErrorKind::WrongValueCount("host".to_string()),
            block.pos,
        );
    }
    let mut names = Vec::new();
    for arg in &block.args {
        let name = host_name(arg).ok_or_else(|| invalid_value("host", arg, block.pos))?;
        if seen.contains(&name) {
            return fail(ConfigErrorKind::DuplicateHost(name), block.pos);
        }
        seen.push(name.clone());
        names.push(name);
    }
    let mut routes: Vec<Route> = Vec::new();
    for item in &block.items {
        match item {
            Item::Setting(setting) => return unknown_key(&setting.key, setting.pos),
            Item::Block(inner) if inner.name == "path" => {
                let route = parse_route(inner)?;
                // Equal segments are the same route (`/api` and `/api/`),
                // compared decoded as the router does (`/@a` and `/%40a`).
                if routes
                    .iter()
                    .any(|r| same_segments(&r.segments, &route.segments))
                {
                    let path = inner.args.first().cloned().unwrap_or_default();
                    return fail(ConfigErrorKind::DuplicatePath(path), inner.pos);
                }
                routes.push(route);
            }
            Item::Block(inner) => return unknown_key(&inner.name, inner.pos),
        }
    }
    if routes.is_empty() {
        return fail(ConfigErrorKind::MissingKey("path"), block.pos);
    }
    Ok(VirtualHost { names, routes })
}

/// Parses and validates configuration text.
pub fn load(text: &str) -> Result<Config, ConfigError> {
    let items = parse(text).map_err(|e| ConfigError {
        kind: ConfigErrorKind::Syntax(e.kind),
        pos: e.pos,
    })?;
    let mut listen: Option<Vec<SocketAddr>> = None;
    let mut user: Option<String> = None;
    let mut limits: Option<Limits> = None;
    let mut hosts: Vec<VirtualHost> = Vec::new();
    let mut seen_hosts: Vec<String> = Vec::new();
    for item in &items {
        match item {
            Item::Setting(setting) => match setting.key.as_str() {
                "listen" => {
                    if listen.is_some() {
                        return duplicate_key("listen", setting.pos);
                    }
                    listen = Some(parse_listen(setting)?);
                }
                "user" => {
                    if user.is_some() {
                        return duplicate_key("user", setting.pos);
                    }
                    let value = one_value(setting)?;
                    if !valid_user(value) {
                        return Err(invalid_value("user", value, setting.pos));
                    }
                    user = Some(value.to_string());
                }
                other => return unknown_key(other, setting.pos),
            },
            Item::Block(block) => match block.name.as_str() {
                "limits" => {
                    if limits.is_some() {
                        return duplicate_key("limits", block.pos);
                    }
                    limits = Some(parse_limits(block)?);
                }
                "host" => hosts.push(parse_host(block, &mut seen_hosts)?),
                other => return unknown_key(other, block.pos),
            },
        }
    }
    let start = Pos { line: 1, column: 1 };
    let Some(listen) = listen else {
        return fail(ConfigErrorKind::MissingKey("listen"), start);
    };
    if hosts.is_empty() {
        return fail(ConfigErrorKind::MissingKey("host"), start);
    }
    Ok(Config {
        listen,
        user,
        limits: limits.unwrap_or_default(),
        hosts,
    })
}

#[cfg(test)]
mod tests;
