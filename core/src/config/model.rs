//! The validated configuration model.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::lexer::Pos;
use super::syntax::SyntaxErrorKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub listen: Vec<SocketAddr>,
    pub user: Option<String>,
    pub limits: Limits,
    pub hosts: Vec<VirtualHost>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_connections: u32,
    pub max_connections_per_ip: u32,
    pub max_request_line: usize,
    pub max_head: usize,
    pub max_headers: usize,
    pub header_timeout: Duration,
    pub keepalive_timeout: Duration,
    pub send_idle_timeout: Duration,
    /// How long a backend connect may take (FR-PROXY-29).
    pub backend_connect_timeout: Duration,
    /// How long a backend may take from the whole request being written
    /// until its response head is complete (FR-PROXY-29).
    pub backend_response_timeout: Duration,
    /// How long a proxied body may make no progress in either direction
    /// (FR-PROXY-29).
    pub body_idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_connections_per_ip: 64,
            max_request_line: 8 * 1024,
            max_head: 16 * 1024,
            max_headers: 100,
            header_timeout: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(60),
            send_idle_timeout: Duration::from_secs(30),
            backend_connect_timeout: Duration::from_secs(5),
            backend_response_timeout: Duration::from_secs(60),
            body_idle_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualHost {
    /// Lowercase host names, IPv4 literals or bracketed IPv6 literals.
    pub names: Vec<String>,
    pub routes: Vec<Route>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The path prefix as segments: `/` is `[]`, `/api/v1` and `/api/v1/`
    /// are `["api", "v1"]`.
    pub segments: Vec<String>,
    pub action: RouteAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteAction {
    Static {
        root: PathBuf,
        on_file_change: OnFileChange,
    },
    Proxy(ProxyRoute),
}

/// What a static route does when a file changes while it is being sent
/// (detected from the stamp the driver reports with every chunk). Either
/// way the headers are already out, so a failure cuts the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnFileChange {
    /// Any change since open (growth, rewrite, shrink) aborts.
    Abort,
    /// A file that only got larger keeps being sent at the size seen at
    /// open; a rewrite at the same size or a shrink still aborts. A rewrite
    /// that also grows the file looks like growth and is accepted.
    AllowGrowth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyRoute {
    /// The backend's IP address and port (FR-CFG-16: a literal, never a
    /// name, so the driver resolves nothing).
    pub backend: SocketAddr,
    pub strip_prefix: StripPrefix,
    pub max_request_body: BodyLimit,
    pub max_response_body: BodyLimit,
}

/// Whether the route's own segments are removed from the request path
/// before it is forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripPrefix {
    Keep,
    Strip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyLimit {
    Bytes(u64),
    Unlimited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigErrorKind {
    Syntax(SyntaxErrorKind),
    UnknownKey(String),
    DuplicateKey(String),
    MissingKey(&'static str),
    InvalidValue { key: String, value: String },
    WrongValueCount(String),
    DuplicateHost(String),
    DuplicatePath(String),
    RouteNeedsOneAction,
    StripPrefixOnStatic,
    BodyLimitOnStatic,
    OnFileChangeOnProxy,
    MissingBodyLimit(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub pos: Pos,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: ", self.pos.line, self.pos.column)?;
        match &self.kind {
            ConfigErrorKind::Syntax(kind) => write!(f, "syntax error ({kind:?})"),
            ConfigErrorKind::UnknownKey(key) => write!(f, "unknown key `{key}`"),
            ConfigErrorKind::DuplicateKey(key) => write!(f, "duplicate key `{key}`"),
            ConfigErrorKind::MissingKey(key) => write!(f, "missing key `{key}`"),
            ConfigErrorKind::InvalidValue { key, value } => {
                write!(f, "invalid value `{value}` for `{key}`")
            }
            ConfigErrorKind::WrongValueCount(key) => {
                write!(f, "wrong number of values for `{key}`")
            }
            ConfigErrorKind::DuplicateHost(name) => write!(f, "duplicate host `{name}`"),
            ConfigErrorKind::DuplicatePath(path) => write!(f, "duplicate path `{path}`"),
            ConfigErrorKind::RouteNeedsOneAction => {
                write!(f, "path needs exactly one of `static` or `proxy`")
            }
            ConfigErrorKind::StripPrefixOnStatic => {
                write!(f, "`strip_prefix` is only allowed on proxy routes")
            }
            ConfigErrorKind::BodyLimitOnStatic => {
                write!(f, "body limits are only allowed on proxy routes")
            }
            ConfigErrorKind::OnFileChangeOnProxy => {
                write!(f, "`on_file_change` is only allowed on static routes")
            }
            ConfigErrorKind::MissingBodyLimit(key) => {
                write!(f, "proxy route is missing `{key}`")
            }
        }
    }
}

impl std::error::Error for ConfigError {}
