//! The fixed scenarios (bench/README.md): what each requests, over which
//! kind of connection, and what a correct response is.

use crate::load::{Expect, Reuse};

/// The host name of the bench config's one site.
pub const HOST: &str = "bench.test";
/// The small file's (and the stub backend's body's) size.
pub const SMALL: u64 = 1024;
/// The large file's size.
pub const LARGE: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    StaticSmall,
    StaticLarge,
    StaticFresh,
    ProxySmall,
    ProxyFresh,
    ConditionalHit,
}

impl Scenario {
    pub const ALL: [Scenario; 6] = [
        Scenario::StaticSmall,
        Scenario::StaticLarge,
        Scenario::StaticFresh,
        Scenario::ProxySmall,
        Scenario::ProxyFresh,
        Scenario::ConditionalHit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::StaticSmall => "static-small",
            Scenario::StaticLarge => "static-large",
            Scenario::StaticFresh => "static-fresh",
            Scenario::ProxySmall => "proxy-small",
            Scenario::ProxyFresh => "proxy-fresh",
            Scenario::ConditionalHit => "conditional-hit",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The request path.
    pub fn path(self) -> &'static str {
        match self {
            Scenario::StaticSmall | Scenario::StaticFresh | Scenario::ConditionalHit => {
                "/small.bin"
            }
            Scenario::StaticLarge => "/large.bin",
            Scenario::ProxySmall | Scenario::ProxyFresh => "/proxy/small",
        }
    }

    pub fn reuse(self) -> Reuse {
        match self {
            Scenario::StaticFresh | Scenario::ProxyFresh => Reuse::Fresh,
            Scenario::StaticSmall
            | Scenario::StaticLarge
            | Scenario::ProxySmall
            | Scenario::ConditionalHit => Reuse::KeepAlive,
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
            Scenario::StaticLarge => LARGE,
            Scenario::StaticSmall
            | Scenario::StaticFresh
            | Scenario::ProxySmall
            | Scenario::ProxyFresh
            | Scenario::ConditionalHit => SMALL,
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
            | Scenario::ProxyFresh => String::new(),
        }
    }
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
    fn conditional_hit_expects_304_only_with_an_etag() {
        let s = Scenario::ConditionalHit;
        assert_eq!(s.expect(Some("\"a\"")).status, 304);
        assert_eq!(s.extra_headers(Some("\"a\"")), "If-None-Match: \"a\"\r\n");
        assert_eq!(s.expect(None).status, 200);
        assert_eq!(s.extra_headers(None), "If-None-Match: \"none\"\r\n");
    }
}
