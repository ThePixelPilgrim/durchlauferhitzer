//! Virtual host and path-prefix routing.

use std::collections::HashMap;

use crate::config::{Route, VirtualHost};

/// Why a request could not be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteMiss {
    UnknownHost,
    NoMatchingPath,
}

#[derive(Clone, Debug)]
pub struct Router {
    hosts: Vec<VirtualHost>,
    by_name: HashMap<String, usize>,
}

impl Router {
    /// Copies the virtual hosts. If a name occurs twice, the first host wins.
    pub fn new(hosts: &[VirtualHost]) -> Router {
        let mut by_name = HashMap::new();
        for (index, host) in hosts.iter().enumerate() {
            for name in &host.names {
                by_name.entry(name.clone()).or_insert(index);
            }
        }
        Router {
            hosts: hosts.to_vec(),
            by_name,
        }
    }

    /// Finds the route for `host` (as sent by the client) and `path`.
    /// The longest matching prefix wins.
    pub fn route(&self, host: Option<&str>, path: &str) -> Result<&Route, RouteMiss> {
        let host = host.ok_or(RouteMiss::UnknownHost)?;
        let key = strip_port(host).to_ascii_lowercase();
        let vhost = self
            .by_name
            .get(&key)
            .and_then(|index| self.hosts.get(*index))
            .ok_or(RouteMiss::UnknownHost)?;
        vhost
            .routes
            .iter()
            .filter(|route| prefix_matches(&route.prefix, path))
            .max_by_key(|route| route.prefix.len())
            .ok_or(RouteMiss::NoMatchingPath)
    }
}

/// Strips a trailing `:digits` port; for `[v6]:port` only after the `]`.
fn strip_port(host: &str) -> &str {
    let search_from = match host.rfind(']') {
        Some(end) => end,
        None if host.contains('[') => return host,
        None => 0,
    };
    let tail = host.get(search_from..).unwrap_or("");
    match tail.rfind(':') {
        Some(colon) => {
            let port = tail.get(colon + 1..).unwrap_or("");
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                host.get(..search_from + colon).unwrap_or(host)
            } else {
                host
            }
        }
        None => host,
    }
}

/// `prefix` matches `path` iff they are equal, or `path` starts with `prefix`
/// and either `prefix` ends in `/` or the next byte of `path` is `/`.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    match path.as_bytes().strip_prefix(prefix.as_bytes()) {
        Some([]) => true,
        Some([next, ..]) => prefix.ends_with('/') || *next == b'/',
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteAction;
    use std::path::PathBuf;

    fn route(prefix: &str) -> Route {
        Route {
            prefix: prefix.to_string(),
            action: RouteAction::Static {
                root: PathBuf::from(format!("/srv{prefix}")),
            },
        }
    }

    fn host(names: &[&str], prefixes: &[&str]) -> VirtualHost {
        VirtualHost {
            names: names.iter().map(|s| s.to_string()).collect(),
            routes: prefixes.iter().map(|p| route(p)).collect(),
        }
    }

    fn matched(router: &Router, host: Option<&str>, path: &str) -> Option<String> {
        router.route(host, path).ok().map(|r| r.prefix.clone())
    }

    #[test]
    fn longest_prefix_wins() {
        let r = Router::new(&[host(&["a.test"], &["/", "/api/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api/")
        );
        assert_eq!(matched(&r, Some("a.test"), "/other").as_deref(), Some("/"));
    }

    #[test]
    fn longest_prefix_wins_regardless_of_order() {
        let r = Router::new(&[host(&["a.test"], &["/api/", "/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api/")
        );
    }

    #[test]
    fn prefix_matches_on_segment_boundaries() {
        let r = Router::new(&[host(&["a.test"], &["/api/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/").as_deref(),
            Some("/api/")
        );
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api/")
        );
        assert_eq!(
            r.route(Some("a.test"), "/apix").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
        assert_eq!(
            r.route(Some("a.test"), "/api").unwrap_err(),
            RouteMiss::NoMatchingPath
        );

        let r = Router::new(&[host(&["a.test"], &["/api"])]);
        assert_eq!(matched(&r, Some("a.test"), "/api").as_deref(), Some("/api"));
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api")
        );
        assert_eq!(
            r.route(Some("a.test"), "/apix").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
    }

    #[test]
    fn paths_are_case_sensitive() {
        let r = Router::new(&[host(&["a.test"], &["/api/"])]);
        assert_eq!(
            r.route(Some("a.test"), "/API/x").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
    }

    #[test]
    fn host_normalization() {
        let r = Router::new(&[host(&["example.test", "[::1]"], &["/"])]);
        assert!(r.route(Some("EXAMPLE.test:8080"), "/").is_ok());
        assert!(r.route(Some("example.test"), "/").is_ok());
        assert!(r.route(Some("[::1]:8080"), "/").is_ok());
        assert!(r.route(Some("[::1]"), "/").is_ok());
        assert_eq!(
            r.route(Some("example.test."), "/").unwrap_err(),
            RouteMiss::UnknownHost
        );
        assert_eq!(r.route(None, "/").unwrap_err(), RouteMiss::UnknownHost);
        assert_eq!(
            r.route(Some("other.test"), "/").unwrap_err(),
            RouteMiss::UnknownHost
        );
        assert_eq!(
            r.route(Some("[::2]:80"), "/").unwrap_err(),
            RouteMiss::UnknownHost
        );
    }

    #[test]
    fn several_names_reach_the_same_routes() {
        let r = Router::new(&[
            host(&["a.test", "www.a.test"], &["/a/"]),
            host(&["b.test"], &["/b/"]),
        ]);
        assert_eq!(matched(&r, Some("a.test"), "/a/x").as_deref(), Some("/a/"));
        assert_eq!(
            matched(&r, Some("www.a.test:81"), "/a/x").as_deref(),
            Some("/a/")
        );
        assert_eq!(matched(&r, Some("b.test"), "/b/x").as_deref(), Some("/b/"));
        assert_eq!(
            r.route(Some("b.test"), "/a/x").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
    }
}
