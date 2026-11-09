//! The validated configuration model.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::conn::SEND_CAPACITY;
use crate::core::client_recv_capacity;
use crate::http::forward::hints_len;
use crate::http::response::FILE_HEAD_EXTRA;
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
    /// How long an idle pooled backend connection is kept (FR-PROXY-02).
    pub backend_idle_timeout: Duration,
    /// The most idle pooled connections kept per backend (FR-PROXY-02),
    /// 1 to `max_connections`.
    pub backend_idle_max: u32,
    /// Size of the disk worker pool (FR-CFG-20).
    pub disk_threads: u32,
    /// How long a graceful shutdown may take before connections are
    /// aborted (FR-PROC-05).
    pub shutdown_timeout: Duration,
    /// How long the old process's restart drain may take before its
    /// remaining connections are cut (FR-PROC-07).
    pub restart_timeout: Duration,
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
/// - only for [`RouteMix::Proxy`] (the config has a proxy route) an
///   active exchange adds the upstream receive buffer
///   `max(32 KiB, max_head)` (`upstream::recv_capacity`) and the head
///   buffer `max_head + max_headers + 128 + hint_bytes + file_head`
///   (`upstream::head_capacity`).
///
/// Saturating arithmetic: the result never wraps.
pub fn worst_case_per_connection(limits: &Limits, mix: RouteMix) -> u64 {
    let widen = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let client = widen(client_recv_capacity(limits)).saturating_add(widen(SEND_CAPACITY));
    match mix {
        RouteMix::StaticOnly => client,
        RouteMix::Proxy {
            hint_bytes,
            file_head,
        } => client
            .saturating_add(widen(recv_capacity(limits.max_head)))
            .saturating_add(widen(head_capacity(
                limits.max_head,
                limits.max_headers,
                hint_bytes.saturating_add(file_head),
            ))),
    }
}

/// What a config's routes add to each connection's buffers (NFR-RES-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteMix {
    /// No proxy route: no exchange buffers.
    StaticOnly,
    /// Some proxy route. `hint_bytes` is the longest Rack hint lines any
    /// `internal_files` route adds to a request head (FR-PROXY-32;
    /// `x-sendfile-type: X-Sendfile`, 29 bytes, or `x-sendfile-type:
    /// X-Accel-Redirect` and `x-accel-mapping: <root>/=<prefix>`, 56 bytes
    /// plus root and prefix); 0 without `internal_files`. `file_head` is
    /// what the core adds around a backend's kept lines in a
    /// backend-directed file's merged head (FR-PROXY-32,
    /// [`FILE_HEAD_EXTRA`]) when any route has `internal_files`, else 0.
    Proxy { hint_bytes: usize, file_head: usize },
}

impl Config {
    /// The config's [`RouteMix`], computed from its routes (once, at load
    /// and in `Core::new`).
    pub fn route_mix(&self) -> RouteMix {
        let mut mix = RouteMix::StaticOnly;
        for route in self.hosts.iter().flat_map(|h| &h.routes) {
            match &route.action {
                RouteAction::Static { .. } => {}
                RouteAction::Proxy(proxy) => {
                    let hints = proxy.internal_files.as_ref().map_or(0, hints_len);
                    let file = match proxy.internal_files {
                        Some(_) => FILE_HEAD_EXTRA,
                        None => 0,
                    };
                    mix = match mix {
                        RouteMix::StaticOnly => RouteMix::Proxy {
                            hint_bytes: hints,
                            file_head: file,
                        },
                        RouteMix::Proxy {
                            hint_bytes,
                            file_head,
                        } => RouteMix::Proxy {
                            hint_bytes: hint_bytes.max(hints),
                            file_head: file_head.max(file),
                        },
                    };
                }
            }
        }
        mix
    }
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
            backend_idle_timeout: Duration::from_secs(2),
            backend_idle_max: 8,
            disk_threads: 4,
            shutdown_timeout: Duration::from_secs(30),
            restart_timeout: Duration::from_secs(600),
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
    /// Whether backend connections are pooled (FR-PROXY-02): on by
    /// default, `pool off;` for backends that keep per-connection state.
    pub pool: Pooling,
    /// Files this route's backend may direct the server to send
    /// (FR-CFG-26); `None`: the backend's file headers are never honoured.
    pub internal_files: Option<InternalFiles>,
}

/// The `internal_files` setting of a proxy route (FR-CFG-26): a backend's
/// `200` response can name a file below `root` for the server to send
/// instead of its own body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalFiles {
    /// Which response header names the file.
    pub header: FileHeader,
    /// The folder the files are below (FR-CFG-15 syntax, visible ASCII
    /// only). It is opened at startup like a static root.
    pub root: PathBuf,
    /// What happens when a sent file changes (FR-STATIC-12 semantics);
    /// `Abort` unless the route says `on_file_change`.
    pub on_file_change: OnFileChange,
}

/// The response header that names an internal file (FR-CFG-26).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileHeader {
    /// `X-Sendfile`: the value is the file's absolute path below the root.
    XSendfile,
    /// `X-Accel-Redirect`: the value is a path starting with `prefix`;
    /// what follows the prefix is the file's path below the root.
    XAccelRedirect {
        /// Starts and ends with `/`.
        prefix: String,
    },
}

impl FileHeader {
    /// `X-Sendfile`'s name as the response parser lowercases it.
    pub const X_SENDFILE: &'static str = "x-sendfile";
    /// `X-Accel-Redirect`'s name as the response parser lowercases it.
    pub const X_ACCEL_REDIRECT: &'static str = "x-accel-redirect";

    /// The response header the route trusts, as the response parser
    /// lowercases it (FR-PROXY-32 Trigger).
    pub fn response_name(&self) -> &'static str {
        match self {
            FileHeader::XSendfile => Self::X_SENDFILE,
            FileHeader::XAccelRedirect { .. } => Self::X_ACCEL_REDIRECT,
        }
    }

    /// The header's name as Rack's `X-Sendfile-Type` hint names it
    /// (FR-PROXY-32 Hints).
    pub fn hint_value(&self) -> &'static str {
        match self {
            FileHeader::XSendfile => "X-Sendfile",
            FileHeader::XAccelRedirect { .. } => "X-Accel-Redirect",
        }
    }
}

/// Whether a proxy route reuses idle backend connections (FR-PROXY-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    On,
    Off,
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
    /// A setting that does nothing given another (`with` is that setting as
    /// written, e.g. `seccomp off`).
    NoEffect {
        key: String,
        with: String,
    },
    DuplicateHost(String),
    DuplicatePath(String),
    RouteNeedsOneAction,
    StripPrefixOnStatic,
    BodyLimitOnStatic,
    /// `on_file_change` on a proxy route without `internal_files`: it serves
    /// no file the setting could apply to.
    OnFileChangeWithoutFiles,
    InternalFilesOnStatic,
    /// An `internal_files` root equal to or below a static root, compared
    /// component by component (FR-CFG-26): its files would be public.
    InternalFilesUnderStatic {
        internal_root: String,
        static_root: String,
    },
    PoolOnStatic,
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
            ConfigErrorKind::NoEffect { key, with } => {
                write!(f, "`{key}` has no effect with `{with}`")
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
            ConfigErrorKind::OnFileChangeWithoutFiles => {
                write!(
                    f,
                    "`on_file_change` is only allowed on static routes and proxy routes \
                     with `internal_files`"
                )
            }
            ConfigErrorKind::InternalFilesOnStatic => {
                write!(f, "`internal_files` is only allowed on proxy routes")
            }
            ConfigErrorKind::InternalFilesUnderStatic {
                internal_root,
                static_root,
            } => write!(
                f,
                "`internal_files` root `{internal_root}` is equal to or below the static \
                 root `{static_root}`, which would make its files public"
            ),
            ConfigErrorKind::PoolOnStatic => {
                write!(f, "`pool` is only allowed on proxy routes")
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
