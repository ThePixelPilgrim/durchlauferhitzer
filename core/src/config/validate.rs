//! Validation of the syntax tree into the configuration model.

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use super::lexer::Pos;
use super::model::{
    BackendAddr, BodyLimit, Config, ConfigError, ConfigErrorKind, Limits, ProxyRoute, Route,
    RouteAction, VirtualHost,
};
use super::syntax::{Block, Item, Setting, parse};
use super::units::{parse_duration, parse_size};

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

/// The single value of a setting that takes exactly one.
fn one_value(setting: &Setting) -> Result<&str, ConfigError> {
    match setting.values.as_slice() {
        [value] => Ok(value),
        _ => fail(
            ConfigErrorKind::WrongValueCount(setting.key.clone()),
            setting.pos,
        ),
    }
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

fn parse_listen(setting: &Setting) -> Result<Vec<SocketAddr>, ConfigError> {
    setting
        .values
        .iter()
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid_value("listen", value, setting.pos))
        })
        .collect()
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
}

impl RouteKey {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "static" => Some(Self::Static),
            "proxy" => Some(Self::Proxy),
            "strip_prefix" => Some(Self::StripPrefix),
            "max_request_body" => Some(Self::MaxRequestBody),
            "max_response_body" => Some(Self::MaxResponseBody),
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
                limits.max_head = size().ok_or_else(bad)?;
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
    let prefix = match block.args.as_slice() {
        [arg] if arg.starts_with('/') => arg.clone(),
        [arg] => return Err(invalid_value("path", arg, block.pos)),
        _ => {
            return fail(
                ConfigErrorKind::WrongValueCount("path".to_string()),
                block.pos,
            );
        }
    };
    let mut static_root: Option<Slot<PathBuf>> = None;
    let mut backend: Option<Slot<BackendAddr>> = None;
    let mut strip: Option<Slot<String>> = None;
    let mut max_request: Option<Slot<BodyLimit>> = None;
    let mut max_response: Option<Slot<BodyLimit>> = None;
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
        };
        if taken {
            return duplicate_key(name, pos);
        }
        let value = one_value(setting)?;
        let bad = || invalid_value(name, value, pos);
        match key {
            RouteKey::Static => {
                if !valid_root(value) {
                    return Err(bad());
                }
                static_root = Some(Slot {
                    pos,
                    value: PathBuf::from(value),
                });
            }
            RouteKey::Proxy => {
                backend = Some(Slot {
                    pos,
                    value: parse_backend(value).ok_or_else(bad)?,
                });
            }
            RouteKey::StripPrefix => {
                if value.is_empty() {
                    return Err(bad());
                }
                strip = Some(Slot {
                    pos,
                    value: value.to_string(),
                });
            }
            RouteKey::MaxRequestBody => {
                max_request = Some(Slot {
                    pos,
                    value: parse_body_limit(value).ok_or_else(bad)?,
                });
            }
            RouteKey::MaxResponseBody => {
                max_response = Some(Slot {
                    pos,
                    value: parse_body_limit(value).ok_or_else(bad)?,
                });
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
            RouteAction::Static { root: root.value }
        }
        (None, Some(backend)) => {
            if let Some(slot) = &strip {
                if !prefix.starts_with(slot.value.as_str()) {
                    return fail(ConfigErrorKind::StripPrefixNotPrefix, slot.pos);
                }
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
                strip_prefix: strip.map(|slot| slot.value),
                max_request_body: max_request_body.value,
                max_response_body: max_response_body.value,
            })
        }
        (None, None) | (Some(_), Some(_)) => {
            return fail(ConfigErrorKind::RouteNeedsOneAction, block.pos);
        }
    };
    Ok(Route { prefix, action })
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
                if routes.iter().any(|r| r.prefix == route.prefix) {
                    return fail(ConfigErrorKind::DuplicatePath(route.prefix), inner.pos);
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
mod tests {
    use super::super::model::{
        BackendAddr, BodyLimit, Limits, ProxyRoute, Route, RouteAction, VirtualHost,
    };
    use super::super::syntax::SyntaxErrorKind;
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::time::Duration;

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
    strip_prefix /api;
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

    fn static_route(prefix: &str, root: &str) -> Route {
        Route {
            prefix: prefix.to_string(),
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
                    static_route("/", "/srv/www"),
                    Route {
                        prefix: "/api/".to_string(),
                        action: RouteAction::Proxy(ProxyRoute {
                            backend: BackendAddr {
                                host: "127.0.0.1".to_string(),
                                port: 3000,
                            },
                            strip_prefix: Some("/api".to_string()),
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
            }
        );
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
                "strip_prefix" => "/",
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
            (invalid("strip_prefix", ""), 5, 5)
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
                Some(static_route("/", good))
            );
        }
    }

    fn proxy_route(host: &str, port: u16) -> RouteAction {
        RouteAction::Proxy(ProxyRoute {
            backend: BackendAddr {
                host: host.to_string(),
                port,
            },
            strip_prefix: None,
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

    #[test]
    fn strip_prefix_on_static_is_rejected() {
        assert_eq!(
            err(&in_host(
                "  path / {\n    static /s;\n    strip_prefix /;\n  }"
            )),
            (ConfigErrorKind::StripPrefixOnStatic, 5, 5)
        );
    }

    #[test]
    fn strip_prefix_must_be_a_prefix_of_the_path() {
        let rest = "    max_request_body 1;\n    max_response_body 1;";
        assert_eq!(
            err(&proxy_path(&format!("    strip_prefix /app;\n{rest}"))),
            (ConfigErrorKind::StripPrefixNotPrefix, 5, 5)
        );
        assert_eq!(
            err(&proxy_path(&format!("    strip_prefix /api/x;\n{rest}"))),
            (ConfigErrorKind::StripPrefixNotPrefix, 5, 5)
        );
        for good in ["/api", "/api/", "/"] {
            ok(&proxy_path(&format!("    strip_prefix {good};\n{rest}")));
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
    fn display_names_line_and_column() {
        let e = load("listen 127.0.0.1:80;\nhost a { path / { static /s; } }\n    lsiten x;")
            .err()
            .map(|e| e.to_string());
        assert_eq!(e.as_deref(), Some("3:5: unknown key `lsiten`"));
    }
}
