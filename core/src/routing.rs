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

    /// Finds the route for `host` (as sent by the client) and the validated
    /// `path`. A route matches when the path's segments start with the
    /// route's segments; the route with the most segments wins.
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
            .filter(|route| segments_match(&route.segments, path))
            .max_by_key(|route| route.segments.len())
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

/// The non-empty segments of a path. A validated path has no `//`, so only
/// a leading and a trailing `/` produce empty pieces.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|segment| !segment.is_empty())
}

/// Whether the segments of `path` start with `route`.
fn segments_match(route: &[String], path: &str) -> bool {
    let mut path = segments(path);
    route
        .iter()
        .all(|segment| path.next() == Some(segment.as_str()))
}

/// Removes the first `count` segments of a validated path. An empty result
/// is `/`; otherwise a trailing `/` on `path` is kept
/// (`/api/x/`, 1 → `/x/`; `/api/`, 1 → `/`).
pub fn strip_segments(path: &str, count: usize) -> String {
    let mut out = String::with_capacity(path.len());
    for segment in segments(path).skip(count) {
        out.push('/');
        out.push_str(segment);
    }
    if out.is_empty() {
        return "/".to_string();
    }
    if path.ends_with('/') {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteAction;
    use std::path::PathBuf;

    /// A route for `prefix`, split into segments as the config does.
    fn route(prefix: &str) -> Route {
        Route {
            segments: prefix
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
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
        router
            .route(host, path)
            .ok()
            .map(|r| format!("/{}", r.segments.join("/")))
    }

    #[test]
    fn longest_prefix_wins() {
        let r = Router::new(&[host(&["a.test"], &["/", "/api/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api")
        );
        assert_eq!(matched(&r, Some("a.test"), "/other").as_deref(), Some("/"));
    }

    #[test]
    fn longest_prefix_wins_regardless_of_order() {
        let r = Router::new(&[host(&["a.test"], &["/api/", "/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api")
        );
    }

    #[test]
    fn prefix_matches_on_segment_boundaries() {
        let r = Router::new(&[host(&["a.test"], &["/api/"])]);
        assert_eq!(
            matched(&r, Some("a.test"), "/api/").as_deref(),
            Some("/api")
        );
        assert_eq!(
            matched(&r, Some("a.test"), "/api/x").as_deref(),
            Some("/api")
        );
        assert_eq!(
            r.route(Some("a.test"), "/apix").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
        // A trailing `/` on the route is insignificant: `/api/` matches `/api`.
        assert_eq!(matched(&r, Some("a.test"), "/api").as_deref(), Some("/api"));

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
    fn route_matches_whole_segments() {
        for prefix in ["/api", "/api/"] {
            let r = Router::new(&[host(&["a.test"], &[prefix])]);
            for path in ["/api", "/api/", "/api/x", "/api/x/y/"] {
                assert_eq!(
                    matched(&r, Some("a.test"), path).as_deref(),
                    Some("/api"),
                    "route {prefix}, path {path}"
                );
            }
            for path in ["/apix", "/apix/", "/ap", "/", "/x/api"] {
                assert_eq!(
                    r.route(Some("a.test"), path).unwrap_err(),
                    RouteMiss::NoMatchingPath,
                    "route {prefix}, path {path}"
                );
            }
        }
    }

    #[test]
    fn most_segments_wins() {
        let r = Router::new(&[host(&["a.test"], &["/a/b", "/", "/a/b/c", "/a"])]);
        for (path, expected) in [
            ("/a/b/c/d", "/a/b/c"),
            ("/a/b/c", "/a/b/c"),
            ("/a/b/", "/a/b"),
            ("/a/bc", "/a"),
            ("/a", "/a"),
            ("/ab", "/"),
            ("/", "/"),
        ] {
            assert_eq!(
                matched(&r, Some("a.test"), path).as_deref(),
                Some(expected),
                "{path}"
            );
        }
    }

    #[test]
    fn strip_segments_cases() {
        for (path, count, expected) in [
            ("/api/x/", 1, "/x/"),
            ("/api/x", 1, "/x"),
            ("/api/", 1, "/"),
            ("/api", 1, "/"),
            ("/a/b/c", 2, "/c"),
            ("/a/b/c/", 2, "/c/"),
            ("/a/b/c", 3, "/"),
            ("/a/b", 5, "/"),
            ("/a/b/", 0, "/a/b/"),
            ("/", 0, "/"),
            ("/", 1, "/"),
        ] {
            assert_eq!(strip_segments(path, count), expected, "{path} {count}");
        }
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
        assert_eq!(matched(&r, Some("a.test"), "/a/x").as_deref(), Some("/a"));
        assert_eq!(
            matched(&r, Some("www.a.test:81"), "/a/x").as_deref(),
            Some("/a")
        );
        assert_eq!(matched(&r, Some("b.test"), "/b/x").as_deref(), Some("/b"));
        assert_eq!(
            r.route(Some("b.test"), "/a/x").unwrap_err(),
            RouteMiss::NoMatchingPath
        );
    }
}
