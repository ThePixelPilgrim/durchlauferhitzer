//! The validated configuration model.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::conn::SEND_CAPACITY;
use crate::core::client_recv_capacity;
use crate::upstream::{head_capacity, recv_capacity};

use super::lexer::Pos;
use super::syntax::SyntaxErrorKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub listen: Vec<SocketAddr>,
    pub user: Option<String>,
    pub limits: Limits,
    /// How strictly the containment layers apply (FR-CFG-25). The core
    /// only holds it; the driver enforces it.
    pub containment: Containment,
    pub hosts: Vec<VirtualHost>,
}

/// How strictly one containment layer applies (FR-CFG-25).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// Skip the layer.
    Off,
    /// Use it where the kernel supports it; otherwise log and serve.
    BestEffort,
    /// A missing or partly supported layer is a startup error.
    Required,
}

/// What a system call the seccomp filter does not allow gets (FR-CFG-25).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeccompAction {
    /// It returns an error and the server keeps running.
    Fail,
    /// The process ends at once.
    Kill,
}

/// The `containment` block (FR-CFG-25).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Containment {
    pub landlock: Layer,
    pub seccomp: Layer,
    pub seccomp_action: SeccompAction,
}

impl Default for Containment {
    fn default() -> Self {
        Self {
            landlock: Layer::BestEffort,
            seccomp: Layer::BestEffort,
            seccomp_action: SeccompAction::Fail,
        }
    }
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
    /// Size of the disk worker pool (FR-CFG-20).
    pub disk_threads: u32,
    /// How long a graceful shutdown may take before connections are
    /// aborted (FR-PROC-05).
    pub shutdown_timeout: Duration,
    /// The memory budget in bytes: `max_connections` times
    /// [`worst_case_per_connection`] must fit in it (FR-CFG-21). It budgets
    /// each connection's own buffers only. Outside it, and not counted: the
    /// send buffers of refused connections, the 4 KiB lingering scratch
    /// buffer, and driver-side buffers (disk readahead, the log buffer).
    pub memory_limit: u64,
    /// Prefix length in bits that groups IPv6 peers for the per-IP limit
    /// (FR-CONN-13), 32 to 128.
    pub ipv6_prefix: u8,
}

/// The most buffer memory one counted connection can hold, from the
/// NFR-RES-01 formulas the core allocates by. It calls the core's own
/// capacity functions, so the two cannot drift apart:
///
/// - client receive buffer `max(max_head, max_request_line + 2)`
///   (`client_recv_capacity`);
/// - client send buffer 1 KiB (`conn::SEND_CAPACITY`);
/// - only when `proxied` (the config has a proxy route) an active exchange
///   adds the upstream receive buffer `max(16 KiB, max_head)`
///   (`upstream::recv_capacity`) and the head buffer
///   `max_head + max_headers + 128` (`upstream::head_capacity`).
///
/// Saturating arithmetic: the result never wraps.
pub fn worst_case_per_connection(limits: &Limits, proxied: bool) -> u64 {
    let widen = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let client = widen(client_recv_capacity(limits)).saturating_add(widen(SEND_CAPACITY));
    if !proxied {
        return client;
    }
    client
        .saturating_add(widen(recv_capacity(limits.max_head)))
        .saturating_add(widen(head_capacity(limits.max_head, limits.max_headers)))
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
            disk_threads: 4,
            shutdown_timeout: Duration::from_secs(30),
            memory_limit: 1 << 30,
            ipv6_prefix: 64,
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
    InvalidValue {
        key: String,
        value: String,
    },
    WrongValueCount(String),
    DuplicateHost(String),
    DuplicatePath(String),
    RouteNeedsOneAction,
    StripPrefixOnStatic,
    BodyLimitOnStatic,
    OnFileChangeOnProxy,
    MissingBodyLimit(&'static str),
    /// `max_connections` times the worst case per connection exceeds
    /// `memory_limit` (FR-CFG-21).
    MemoryBudget {
        needed: u64,
        limit: u64,
    },
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
            ConfigErrorKind::MemoryBudget { needed, limit } => write!(
                f,
                "memory budget exceeded: max_connections x worst case per connection \
                 needs {needed} bytes but memory_limit is {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
