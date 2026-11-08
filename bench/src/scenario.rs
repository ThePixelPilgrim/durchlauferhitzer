//! The fixed scenarios (bench/README.md): what each requests, over which
//! kind of connection, and what a correct response is.

use crate::load::{Expect, Reuse};

/// The host name of the bench config's one site.
pub const HOST: &str = "bench.test";
/// The small file's (and the stub backend's body's) size.
pub const SMALL: u64 = 1024;
/// The large file's (and `proxy-large`'s body's) size.
pub const LARGE: u64 = 1024 * 1024;
/// `proxy-medium`'s body size.
pub const MEDIUM: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    StaticSmall,
    StaticLarge,
    StaticFresh,
    ProxySmall,
    ProxyFresh,
    ConditionalHit,
    StaticSmall1kIdle,
    ProxyMedium,
    ProxyLarge,
    ProxyFile,
}

/// `proxy-file`'s request path: the `/file` route (`strip_prefix`, so
/// the backend sees `/1k`), whose backend names the 1 KiB file
/// (FR-PROXY-32, [`crate::backend`]).
pub const FILE_PATH: &str = "/file/1k";

/// The idle keep-alive connections `static-small-1k-idle` holds open.
pub const IDLE: usize = 1000;

impl Scenario {
    /// Every scenario.
    pub const ALL: [Scenario; 10] = [
        Scenario::StaticSmall,
        Scenario::StaticLarge,
        Scenario::StaticFresh,
        Scenario::ProxySmall,
        Scenario::ProxyFresh,
        Scenario::ConditionalHit,
        Scenario::StaticSmall1kIdle,
        Scenario::ProxyMedium,
        Scenario::ProxyLarge,
        Scenario::ProxyFile,
    ];

    /// The scenarios run without `--scenarios` (the nightly's set):
    /// all but `static-small-1k-idle`, `proxy-medium` and `proxy-large`,
    /// which are run on demand.
    pub const DEFAULT: [Scenario; 7] = [
        Scenario::StaticSmall,
        Scenario::StaticLarge,
        Scenario::StaticFresh,
        Scenario::ProxySmall,
        Scenario::ProxyFresh,
        Scenario::ConditionalHit,
        Scenario::ProxyFile,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::StaticSmall => "static-small",
            Scenario::StaticLarge => "static-large",
            Scenario::StaticFresh => "static-fresh",
            Scenario::ProxySmall => "proxy-small",
            Scenario::ProxyFresh => "proxy-fresh",
            Scenario::ConditionalHit => "conditional-hit",
            Scenario::StaticSmall1kIdle => "static-small-1k-idle",
            Scenario::ProxyMedium => "proxy-medium",
            Scenario::ProxyLarge => "proxy-large",
            Scenario::ProxyFile => "proxy-file",
        }
    }

    /// The routes of the server the scenario runs on.
    pub fn routes(self) -> Routes {
        match self {
            Scenario::ProxyFile => Routes::WithFiles,
            Scenario::StaticSmall
            | Scenario::StaticLarge
            | Scenario::StaticFresh
            | Scenario::ProxySmall
            | Scenario::ProxyFresh
            | Scenario::ConditionalHit
            | Scenario::StaticSmall1kIdle
            | Scenario::ProxyMedium
            | Scenario::ProxyLarge => Routes::Plain,
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The request path.
    pub fn path(self) -> &'static str {
        match self {
            Scenario::StaticSmall
            | Scenario::StaticFresh
            | Scenario::ConditionalHit
            | Scenario::StaticSmall1kIdle => "/small.bin",
            Scenario::StaticLarge => "/large.bin",
            Scenario::ProxySmall | Scenario::ProxyFresh => "/proxy/small",
            Scenario::ProxyMedium => "/proxy/medium",
            Scenario::ProxyLarge => "/proxy/large",
            Scenario::ProxyFile => FILE_PATH,
        }
    }

    pub fn reuse(self) -> Reuse {
        match self {
            Scenario::StaticFresh | Scenario::ProxyFresh => Reuse::Fresh,
            Scenario::StaticSmall
            | Scenario::StaticLarge
            | Scenario::ProxySmall
            | Scenario::ConditionalHit
            | Scenario::StaticSmall1kIdle
            | Scenario::ProxyMedium
            | Scenario::ProxyLarge
            | Scenario::ProxyFile => Reuse::KeepAlive,
        }
    }

    /// Idle keep-alive connections held open (each after one request)
    /// while the scenario runs, its measured window and strace pass.
    pub fn idle(self) -> usize {
        match self {
            Scenario::StaticSmall1kIdle => IDLE,
            Scenario::StaticSmall
            | Scenario::StaticLarge
            | Scenario::StaticFresh
            | Scenario::ProxySmall
            | Scenario::ProxyFresh
            | Scenario::ConditionalHit
            | Scenario::ProxyMedium
            | Scenario::ProxyLarge
            | Scenario::ProxyFile => 0,
        }
    }

    /// A correct response. `conditional-hit` sends the `ETag` the server
    /// gave (`etag`): a match is a 304; a server without `ETag`s (before
    /// FR-STATIC-18) is asked with a tag that never matches, and its 200
    /// is correct.
    pub fn expect(self, etag: Option<&str>) -> Expect {
        let status = match (self, etag) {
            (Scenario::ConditionalHit, Some(_)) => 304,
            _ => 200,
        };
        let body_len = match self {
            Scenario::StaticLarge | Scenario::ProxyLarge => LARGE,
            Scenario::ProxyMedium => MEDIUM,
            Scenario::StaticSmall
            | Scenario::StaticFresh
            | Scenario::ProxySmall
            | Scenario::ProxyFresh
            | Scenario::ConditionalHit
            | Scenario::StaticSmall1kIdle
            | Scenario::ProxyFile => SMALL,
        };
        Expect { status, body_len }
    }

    /// The extra request header lines.
    pub fn extra_headers(self, etag: Option<&str>) -> String {
        match self {
            Scenario::ConditionalHit => {
                format!("If-None-Match: {}\r\n", etag.unwrap_or("\"none\""))
            }
            Scenario::StaticSmall
            | Scenario::StaticLarge
            | Scenario::StaticFresh
            | Scenario::ProxySmall
            | Scenario::ProxyFresh
            | Scenario::StaticSmall1kIdle
            | Scenario::ProxyMedium
            | Scenario::ProxyLarge
            | Scenario::ProxyFile => String::new(),
        }
    }
}

/// Which routes the bench config has (`server::config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routes {
    /// The static root and the `/proxy` route: every scenario but
    /// `proxy-file`, with the config it ran before stage 8.
    Plain,
    /// Also `/file`, a proxy route with `internal_files` (FR-PROXY-32),
    /// for `proxy-file` alone: its server is its own, since the route
    /// grows every connection's head buffer (NFR-RES-01).
    WithFiles,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::from_name(s.name()), Some(s));
        }
        assert_eq!(Scenario::from_name("static"), None);
    }

    #[test]
    fn only_the_idle_scenario_holds_connections_and_it_is_on_demand() {
        for s in Scenario::ALL {
            let expected = match s {
                Scenario::StaticSmall1kIdle => IDLE,
                _ => 0,
            };
            assert_eq!(s.idle(), expected, "{}", s.name());
            let on_demand = matches!(
                s,
                Scenario::StaticSmall1kIdle | Scenario::ProxyMedium | Scenario::ProxyLarge
            );
            assert_eq!(Scenario::DEFAULT.contains(&s), !on_demand, "{}", s.name());
        }
        let s = Scenario::StaticSmall1kIdle;
        assert_eq!(s.path(), Scenario::StaticSmall.path());
        assert_eq!(s.reuse(), Reuse::KeepAlive);
        assert_eq!(s.expect(None), Scenario::StaticSmall.expect(None));
    }

    #[test]
    fn the_large_proxy_scenarios_ask_the_backend_for_their_size() {
        for (s, path, len) in [
            (Scenario::ProxySmall, "/proxy/small", SMALL),
            (Scenario::ProxyMedium, "/proxy/medium", MEDIUM),
            (Scenario::ProxyLarge, "/proxy/large", LARGE),
        ] {
            assert_eq!(s.path(), path);
            assert_eq!(s.expect(None).body_len, len);
            assert_eq!(s.reuse(), Reuse::KeepAlive);
            assert_eq!(
                crate::backend::body_len(path.trim_start_matches("/proxy")),
                len
            );
        }
        assert!(!Scenario::DEFAULT.contains(&Scenario::ProxyMedium));
        assert!(!Scenario::DEFAULT.contains(&Scenario::ProxyLarge));
    }

    /// `proxy-file` (FR-PROXY-32): the 1 KiB file, keep-alive, run by
    /// default (the nightly records it); only it adds the `/file` route.
    #[test]
    fn proxy_file_serves_the_small_file_by_default() {
        let s = Scenario::ProxyFile;
        assert_eq!(Scenario::from_name("proxy-file"), Some(s));
        assert_eq!(s.path(), "/file/1k");
        assert_eq!(s.reuse(), Reuse::KeepAlive);
        assert_eq!(s.idle(), 0);
        assert_eq!(
            s.expect(None),
            Expect {
                status: 200,
                body_len: SMALL
            }
        );
        assert!(Scenario::DEFAULT.contains(&s));
        for other in Scenario::ALL {
            let routes = if other == s {
                Routes::WithFiles
            } else {
                Routes::Plain
            };
            assert_eq!(other.routes(), routes, "{}", other.name());
        }
    }

    #[test]
    fn conditional_hit_expects_304_only_with_an_etag() {
        let s = Scenario::ConditionalHit;
        assert_eq!(s.expect(Some("\"a\"")).status, 304);
        assert_eq!(s.extra_headers(Some("\"a\"")), "If-None-Match: \"a\"\r\n");
        assert_eq!(s.expect(None).status, 200);
        assert_eq!(s.extra_headers(None), "If-None-Match: \"none\"\r\n");
    }
}
