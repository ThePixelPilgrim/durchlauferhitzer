//! File-name extension to `Content-Type` mapping.

const TABLE: &[(&[&str], &str)] = &[
    (&["html", "htm"], "text/html; charset=utf-8"),
    (&["css"], "text/css; charset=utf-8"),
    (&["js", "mjs"], "text/javascript; charset=utf-8"),
    (&["json"], "application/json"),
    (&["txt"], "text/plain; charset=utf-8"),
    (&["xml"], "application/xml"),
    (&["svg"], "image/svg+xml"),
    (&["png"], "image/png"),
    (&["jpg", "jpeg"], "image/jpeg"),
    (&["gif"], "image/gif"),
    (&["webp"], "image/webp"),
    (&["avif"], "image/avif"),
    (&["ico"], "image/x-icon"),
    (&["woff"], "font/woff"),
    (&["woff2"], "font/woff2"),
    (&["pdf"], "application/pdf"),
    (&["wasm"], "application/wasm"),
    (&["mp4"], "video/mp4"),
    (&["webm"], "video/webm"),
];

const FALLBACK: &str = "application/octet-stream";

/// Content type for a file name, by the extension after the last `.`
/// (ASCII case-insensitive). No extension, or only a leading dot, yields
/// `application/octet-stream`.
pub fn content_type(file_name: &[u8]) -> &'static str {
    let Some(dot) = file_name.iter().rposition(|&b| b == b'.') else {
        return FALLBACK;
    };
    if dot == 0 {
        return FALLBACK;
    }
    let ext = file_name.get(dot + 1..).unwrap_or_default();
    for (exts, ty) in TABLE {
        if exts.iter().any(|e| e.as_bytes().eq_ignore_ascii_case(ext)) {
            return ty;
        }
    }
    FALLBACK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_row() {
        let rows: &[(&str, &str)] = &[
            ("a.html", "text/html; charset=utf-8"),
            ("a.htm", "text/html; charset=utf-8"),
            ("a.css", "text/css; charset=utf-8"),
            ("a.js", "text/javascript; charset=utf-8"),
            ("a.mjs", "text/javascript; charset=utf-8"),
            ("a.json", "application/json"),
            ("a.txt", "text/plain; charset=utf-8"),
            ("a.xml", "application/xml"),
            ("a.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("a.jpg", "image/jpeg"),
            ("a.jpeg", "image/jpeg"),
            ("a.gif", "image/gif"),
            ("a.webp", "image/webp"),
            ("a.avif", "image/avif"),
            ("a.ico", "image/x-icon"),
            ("a.woff", "font/woff"),
            ("a.woff2", "font/woff2"),
            ("a.pdf", "application/pdf"),
            ("a.wasm", "application/wasm"),
            ("a.mp4", "video/mp4"),
            ("a.webm", "video/webm"),
        ];
        for (name, want) in rows {
            assert_eq!(content_type(name.as_bytes()), *want, "{name}");
        }
    }

    #[test]
    fn case_insensitive_and_last_dot() {
        assert_eq!(content_type(b"index.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type(b"a.b.Css"), "text/css; charset=utf-8");
        assert_eq!(content_type(b"a.css.gz"), "application/octet-stream");
    }

    #[test]
    fn fallbacks() {
        for name in [
            &b"a.tar.gz"[..],
            b"noext",
            b".x",
            b"",
            b"a.",
            b"a.\xff",
            b"a.htmlx",
        ] {
            assert_eq!(content_type(name), "application/octet-stream", "{name:?}");
        }
    }
}
