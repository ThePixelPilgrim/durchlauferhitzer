# Stage 1: Foundations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** A sans-IO core that loads the config, accepts simulated connections, parses HTTP/1.1 request heads strictly, routes by virtual host and path prefix, answers with fixed responses, and enforces limits and timeouts — all driven and tested through a simulated network and clock.

**Architecture:** Cargo workspace with two crates. `durchlauferhitzer-core` (library, no dependencies, no `unsafe`) holds config, HTTP parsing, routing and the connection state machine, exposed as `Core::handle(now, event, &mut actions)` plus per-connection receive/send buffers owned by the core. `durchlauferhitzer-sim` (test-only, `publish = false`) drives a `Core` over a virtual network and clock with fault injection; end-to-end tests live in its `tests/` directory.

**Tech Stack:** Rust 2024 edition, stable toolchain (1.97 in CI), std only in the product. Test-only: `proptest` (property tests), `fastrand` (seeded randomized simulation).

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` — read it before any task.

## Global Constraints

- `durchlauferhitzer-core` has zero `[dependencies]` and no build script. CI fails otherwise.
- No C in the product. Test-only crates may use established helper crates.
- `#![forbid(unsafe_code)]` everywhere in this stage (workspace lint `unsafe_code = "forbid"`).
- Denied clippy lints in all non-test code: `wildcard_enum_match_arm`, `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`. Untrusted input yields error enums, never panics. Tests may use them (`clippy.toml` `allow-*-in-tests`; the sim crate allows them at crate level).
- Strict red/green TDD: every step that adds behavior is preceded by a failing test that was run and seen to fail.
- No `_ =>` arms on enums anywhere, including in tests' production helpers.
- Use enums instead of `bool` parameters and flag pairs in public APIs.
- No unbounded buffers: every per-connection buffer has a fixed capacity derived from `Limits`.
- No `Server` header and no `Date` header in this stage (the core has no wall clock; `Date` arrives with the real driver).
- Out of scope for stage 1: reading request bodies, static file serving, proxying (both answer 501), the real `epoll` driver, fuzzing (stage 5).

## Review Focus

1. Percent-encoded dots (`%2e`, `%2E`, `%2e%2E`) in paths — must be rejected with 400, because stage 2 percent-decodes for static serving. Test in Task 4.
2. Closing a connection while the client's unread body is still arriving — a plain close sends a TCP reset that can destroy the response before the client reads it; the core must shut down writing and drain (lingering close). Test in Task 9.
3. A request head split at every possible byte boundary, and two pipelined requests arriving in one read — both must produce exactly the same responses as a single clean read. Test in Task 10.
4. Bare `LF` line endings, a lone `CR` or `NUL` inside a header value, and whitespace between header name and colon — 400, never accepted. Test in Task 5.
5. `Host` values with a port, uppercase letters, an IPv6 literal, or a trailing dot — the first three route correctly, the trailing dot does not match (404). Test in Task 6.

---

## File Structure

```
Cargo.toml                         workspace, shared lints, profile
clippy.toml                        allow unwrap/expect/panic/indexing in tests
rust-toolchain.toml                channel = "1.97"
.github/workflows/ci.yml           fmt, clippy, test, product dependency check
scripts/check-product-deps.sh      fails if the product tree has non-workspace crates (allow-list: libc)
core/Cargo.toml                    package durchlauferhitzer-core
core/src/lib.rs                    module declarations, re-exports
core/src/time.rs                   Instant (monotonic, driver-supplied)
core/src/config/mod.rs             load(text) -> Result<Config, ConfigError>
core/src/config/lexer.rs           tokens with line/column
core/src/config/syntax.rs          token stream -> tree of settings and blocks
core/src/config/model.rs           Config, Limits, VirtualHost, Route, RouteAction, ...
core/src/config/validate.rs        tree -> Config, all semantic rules
core/src/config/units.rs           durations and sizes
core/src/http/mod.rs
core/src/http/target.rs            origin-form target validation
core/src/http/request.rs           incremental request-head parser
core/src/http/status.rs            Status enum
core/src/http/response.rs          fixed response rendering
core/src/routing.rs                Router: host + longest prefix
core/src/core.rs                   Core, Event, Action, Interest, ConnId
core/src/conn.rs                   per-connection state enum and transitions
core/src/log.rs                    Log enum and reasons
sim/Cargo.toml                     package durchlauferhitzer-sim, publish = false
sim/src/lib.rs                     Sim: virtual network + clock + invariant checks
sim/tests/routing.rs               end-to-end request/response tests
sim/tests/limits.rs                timeouts, connection caps, lingering close
sim/tests/faults.rs                byte splitting, pipelining, resets, random seeds
```

---

### Task 1: Workspace, lints, CI and config lexer

**Files:**
- Create: `Cargo.toml`, `clippy.toml`, `rust-toolchain.toml`, `.github/workflows/ci.yml`, `scripts/check-product-deps.sh`, `core/Cargo.toml`, `core/src/lib.rs`, `core/src/config/mod.rs`, `core/src/config/lexer.rs`
- Modify: `CLAUDE.md` (add a "Development" section: strict red/green TDD; run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `scripts/check-product-deps.sh` before every commit)

**Interfaces:**
- Produces:
  - `pub struct Pos { pub line: u32, pub column: u32 }` (1-based) in `config::lexer`, `Copy, Debug, PartialEq, Eq`
  - `pub enum TokenKind { Word(String), Colon, Semicolon, OpenBrace, CloseBrace }`
  - `pub enum Spacing { Joined, Separated }` — whether the token directly follows the previous one, or whitespace/a comment/start of input precedes it
  - `pub struct Token { pub kind: TokenKind, pub spacing: Spacing, pub pos: Pos }`
  - `pub enum LexErrorKind { UnexpectedChar(char) }`, `pub struct LexError { pub kind: LexErrorKind, pub pos: Pos }`
  - `pub fn tokenize(text: &str) -> Result<Vec<Token>, LexError>`

Workspace `Cargo.toml`: `resolver = "3"`, members `core`, `sim`; `[workspace.package] edition = "2024"`, `rust-version = "1.85"`; `[workspace.lints.rust] unsafe_code = "forbid"`; `[workspace.lints.clippy]` the five denied lints from Global Constraints at `"deny"`. Each crate sets `[lints] workspace = true`. `clippy.toml`: `allow-unwrap-in-tests`, `allow-expect-in-tests`, `allow-panic-in-tests`, `allow-indexing-slicing-in-tests` all `true`. `check-product-deps.sh` lists `cargo tree -p durchlauferhitzer-core -e normal,build --prefix none --format '{p}'`, and exits 1 naming every package that is neither `durchlauferhitzer-*` nor in `ALLOWED=(libc)`. CI runs fmt, clippy, test and the script on `ubuntu-latest` on push and pull request. Create `sim/` in Task 8; until then the workspace lists only `core`.

Lexer rules: whitespace separates tokens; `#` starts a comment to end of line; `:` `;` `{` `}` are single tokens; a word is a maximal run of other non-whitespace characters. Control characters other than `\t`, `\n`, `\r` → `UnexpectedChar`.

- [ ] **Step 1: Write the failing tests** in `core/src/config/lexer.rs`

```rust
#[test]
fn tokenizes_setting_with_positions() {
    let t = tokenize("listen: 127.0.0.1:8080;").unwrap();
    // ":" inside a word is still a separator token
    assert_eq!(kinds(&t), vec![w("listen"), Colon, w("127.0.0.1"), Colon, w("8080"), Semicolon]);
    assert_eq!(t[0].pos, Pos { line: 1, column: 1 });
    assert_eq!(t[2].pos, Pos { line: 1, column: 9 });
    assert_eq!(t[2].spacing, Spacing::Separated);
    assert_eq!(t[3].spacing, Spacing::Joined);
}
#[test]
fn skips_comments_and_tracks_lines() {
    let t = tokenize("# c\nhost a.test {\n}\n").unwrap();
    assert_eq!(kinds(&t), vec![w("host"), w("a.test"), OpenBrace, CloseBrace]);
    assert_eq!(t[3].pos, Pos { line: 3, column: 1 });
}
#[test]
fn rejects_control_characters() {
    let e = tokenize("a\u{0}b").unwrap_err();
    assert_eq!(e, LexError { kind: LexErrorKind::UnexpectedChar('\u{0}'), pos: Pos { line: 1, column: 2 } });
}
```

`:` is its own token, so `127.0.0.1:8080` lexes as `Word Colon Word`; Task 2 rejoins `Joined` tokens into one value.

- [ ] **Step 2: Run to verify they fail**
  Run: `cargo test -p durchlauferhitzer-core config::lexer`
  Expected: compile error / FAIL (`tokenize` not defined).

- [ ] **Step 3: Implement the scaffold files and `tokenize`** as specified above.

- [ ] **Step 4: Run all checks**
  Run: `cargo test -p durchlauferhitzer-core config::lexer && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && scripts/check-product-deps.sh`
  Expected: all PASS, script prints nothing and exits 0.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "Add workspace, lints, CI and config lexer"`

---

### Task 1b: Quoted strings in the lexer (amendment)

**Files:**
- Modify: `core/src/config/lexer.rs`

**Interfaces:**
- Consumes: Task 1's lexer.
- Produces:
  - new variant `TokenKind::Quoted(String)` (the unescaped contents)
  - new variants `LexErrorKind::UnterminatedString`, `LexErrorKind::BadEscape(char)`

Rules: `"` starts a string that ends at the next unescaped `"`. Inside, `\"` → `"` and `\\` → `\`; any other character after `\` → `BadEscape(c)` at the backslash's position. A newline or end of input before the closing quote → `UnterminatedString` at the opening quote's position. Control characters inside a string → `UnexpectedChar` as elsewhere. `#`, `:`, `;`, `{`, `}` and spaces inside a string are ordinary characters. A `"` also ends a preceding word (it is a delimiter like `#`). `Spacing` applies to `Quoted` tokens as to any other.

- [ ] **Step 1: Write the failing tests** — `quoted_string_keeps_special_characters` (`r#"a: "x#y; {z}";"#` → `[w("a"), Colon, Quoted("x#y; {z}"), Semicolon]`); `quoted_string_escapes` (`r#""a\"b\\c""#` → `Quoted("a\"b\\c")` i.e. contents `a"b\c`); `unterminated_string_reports_opening_quote` (`"a \"bc\nd\""` → `UnterminatedString` at 1:3); `bad_escape_reports_backslash` (`r#""a\nb""#` → `BadEscape('n')` at 1:3); `quote_ends_word` (`r#"ab"c""#` → `[w("ab"), Quoted("c")]` with the quoted token `Joined`).
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core config::lexer` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run all checks** — `cargo test -p durchlauferhitzer-core config::lexer && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && scripts/check-product-deps.sh` → PASS.
- [ ] **Step 5: Commit** — `"Add quoted strings to the config lexer"`

---

### Task 1c: Drop the colon token and spacing (amendment)

The grammar changed after Tasks 1/1b: settings are `key value…;` with no colon, and `:` is an ordinary word character (no splitting and re-gluing). This task removes what that made obsolete.

**Files:**
- Modify: `core/src/config/lexer.rs`

**Interfaces:**
- Produces (final lexer API): `pub enum TokenKind { Word(String), Quoted(String), Semicolon, OpenBrace, CloseBrace }`, `pub struct Token { pub kind: TokenKind, pub pos: Pos }`, `LexErrorKind`, `LexError`, `Pos`, `tokenize` unchanged. `TokenKind::Colon`, `Spacing` and `Token::spacing` are removed.

- [ ] **Step 1: Change the tests first** — `tokenizes_setting_with_positions` becomes `tokenize("listen 127.0.0.1:8080 [::1]:80;")` → `[w("listen"), w("127.0.0.1:8080"), w("[::1]:80"), Semicolon]` with `t[1].pos == Pos { line: 1, column: 8 }`; `quoted_string_keeps_special_characters` input becomes `r#"a "x#y; {z}";"#` → `[w("a"), Quoted("x#y; {z}"), Semicolon]`; `quote_ends_word` keeps `[w("ab"), Quoted("c")]` without spacing assertions; `hash_inside_word_comment_ends_at_newline` drops its spacing assertion; delete every other `Spacing` assertion.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core config::lexer` → FAIL (colon still split / spacing field present).
- [ ] **Step 3: Implement** — remove `Colon`, `Spacing`, `Token::spacing`; `:` is no longer a delimiter.
- [ ] **Step 4: Run all checks** — `cargo test -p durchlauferhitzer-core config::lexer && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && scripts/check-product-deps.sh` → PASS.
- [ ] **Step 5: Commit** — `"Simplify config lexer: colon is an ordinary character"`

---

### Task 2: Config syntax tree

**Files:**
- Create: `core/src/config/syntax.rs`

**Interfaces:**
- Consumes: `tokenize`, `Token`, `TokenKind`, `Pos` (Tasks 1, 1b, 1c)
- Produces:
  - `pub struct Setting { pub key: String, pub values: Vec<String>, pub pos: Pos }`
  - `pub struct Block { pub name: String, pub args: Vec<String>, pub items: Vec<Item>, pub pos: Pos }`
  - `pub enum Item { Setting(Setting), Block(Block) }`
  - `pub enum SyntaxErrorKind { Lex(LexErrorKind), UnexpectedToken, UnexpectedEnd, UnclosedBlock, EmptyValue }`
  - `pub struct SyntaxError { pub kind: SyntaxErrorKind, pub pos: Pos }`
  - `pub fn parse(text: &str) -> Result<Vec<Item>, SyntaxError>`

Grammar from the spec. A setting is `Word value+ Semicolon`; a block is `Word arg* OpenBrace Item* CloseBrace`; a value or arg is one `Word` or `Quoted` token. Keys and block names must be `Word`s. Which form an item is gets decided by the first `Semicolon` or `OpenBrace` after its name. `EmptyValue` for `key;`. `UnclosedBlock` reports the block's opening position. Nesting depth is capped at 8 (`UnexpectedToken` beyond) so hostile configs cannot recurse deeply.

- [ ] **Step 1: Write the failing tests** in `syntax.rs`: `parses_nested_blocks` (the spec's example config: 3 top-level items; `host` block args `["example.com", "www.example.com"]`; its second `path` block has args `["/api/"]` and settings `proxy` = `["127.0.0.1:3000"]`, `strip_prefix` = `["/api"]`); `multiple_values` (`listen [::1]:8080 127.0.0.1:80;` → values `["[::1]:8080", "127.0.0.1:80"]`); `quoted_values` (`a "x y" z;` → values `["x y", "z"]`); `rejects_quoted_key` (`"a" b;` → `UnexpectedToken` at 1:1); `reports_unclosed_block_at_its_start` (`"a {\n b c;\n"` → `UnclosedBlock` at 1:1); `rejects_missing_semicolon` (`"a { b c }"` → `UnexpectedToken` at 1:9); `rejects_empty_value`; `rejects_nesting_deeper_than_8`.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core config::syntax` → FAIL.
- [ ] **Step 3: Implement `parse`** as a recursive-descent parser with an explicit depth counter.
- [ ] **Step 4: Run to verify they pass** — same command → PASS; clippy clean.
- [ ] **Step 5: Commit** — `"Add config syntax tree parser"`

---

### Task 3: Config validation and model

**Files:**
- Create: `core/src/config/model.rs`, `core/src/config/units.rs`, `core/src/config/validate.rs`
- Modify: `core/src/config/mod.rs`

**Interfaces:**
- Consumes: `parse`, `Item`, `Setting`, `Block`, `SyntaxError` (Task 2)
- Produces (all in `durchlauferhitzer_core::config`):
  - `pub fn parse_duration(s: &str) -> Option<Duration>` — `<u64>ms|s|m`, no other forms
  - `pub fn parse_size(s: &str) -> Option<u64>` — plain bytes or `<u64>KiB|MiB|GiB`, overflow → `None`
  - `pub struct Config { pub listen: Vec<SocketAddr>, pub user: Option<String>, pub limits: Limits, pub hosts: Vec<VirtualHost> }`
  - `pub struct Limits { pub max_connections: u32, pub max_connections_per_ip: u32, pub max_request_line: usize, pub max_head: usize, pub max_headers: usize, pub header_timeout: Duration, pub keepalive_timeout: Duration, pub send_idle_timeout: Duration }` with `impl Default`
  - `pub struct VirtualHost { pub names: Vec<String>, pub routes: Vec<Route> }` (names lowercase)
  - `pub struct Route { pub prefix: String, pub action: RouteAction }`
  - `pub enum RouteAction { Static { root: PathBuf }, Proxy(ProxyRoute) }`
  - `pub struct ProxyRoute { pub backend: BackendAddr, pub strip_prefix: Option<String>, pub max_request_body: BodyLimit, pub max_response_body: BodyLimit }`
  - `pub struct BackendAddr { pub host: String, pub port: u16 }` (resolved later, by the driver)
  - `pub enum BodyLimit { Bytes(u64), Unlimited }`
  - `pub enum ConfigErrorKind { Syntax(SyntaxErrorKind), UnknownKey(String), DuplicateKey(String), MissingKey(&'static str), InvalidValue { key: String, value: String }, WrongValueCount(String), DuplicateHost(String), DuplicatePath(String), RouteNeedsOneAction, StripPrefixOnStatic, StripPrefixNotPrefix, BodyLimitOnStatic, MissingBodyLimit(&'static str) }`
  - `pub struct ConfigError { pub kind: ConfigErrorKind, pub pos: Pos }` with `Display` as `"{line}:{column}: {message}"`
  - `pub fn load(text: &str) -> Result<Config, ConfigError>`

Defaults for `Limits` (stage 1 keys only; later stages add keys):

| key | default |
|---|---|
| `max_connections` | 1024 |
| `max_connections_per_ip` | 64 |
| `max_request_line` | 8KiB |
| `max_head` | 16KiB (request line + all headers + final CRLF) |
| `max_headers` | 100 |
| `header_timeout` | 10s |
| `keepalive_timeout` | 60s |
| `send_idle_timeout` | 30s |

Rules (each violation is a `ConfigError` at the offending item's position): top level allows only `listen` (≥1 IP `SocketAddr`, required), `user` (one value, optional), one `limits` block, `host` blocks (≥1 required). `host` takes ≥1 name args (lowercased; ASCII letters, digits, `-`, `.`; no leading/trailing `.`; or an IPv4 literal, or an IPv6 literal in brackets such as `[::1]`) and only `path` blocks inside. `path` takes exactly one arg that starts with `/` and passes `validate_path` (Task 4 — until Task 4 lands, check only the leading `/`; Task 4 Step 3 wires in the full check). A route needs exactly one of `static` (absolute path) / `proxy` (`host:port`); `strip_prefix` only on proxy and must be a prefix of `path`'s arg; proxy routes require both `max_request_body` and `max_response_body` (`parse_size` or `unlimited`); static routes may have neither. `limits` must be ≥ 1 for every count and size and non-zero for every duration; `max_request_line` ≤ `max_head`. Duplicate keys anywhere, a host name in two blocks, or two identical `path` args in one host are errors.

- [ ] **Step 1: Write the failing tests**
  - `units.rs`: `parse_duration("10s") == Some(10s)`, `("250ms")`, `("2m")`, `("10")`, `("1h")`, `("s")` → `None`; `parse_size("16KiB") == Some(16384)`, `("1GiB")`, `("4096")`, `("1kb")` → `None`, `("18446744073709551615GiB")` → `None`.
  - `validate.rs`: `loads_spec_example` (the spec's example config, including its `limits` block and body limits → the expected `Config` value in full); one test per rule above asserting the exact `ConfigErrorKind` and position, e.g. `proxy_route_without_max_request_body_is_rejected` → `MissingBodyLimit("max_request_body")` at the `path` block's position; `display_names_line_and_column` → `"3:5: unknown key `lsiten`"`.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core config` → FAIL.
- [ ] **Step 3: Implement** `units.rs`, `model.rs`, `validate.rs`, `load`.
- [ ] **Step 4: Run to verify they pass** — same command → PASS; clippy clean.
- [ ] **Step 5: Commit** — `"Add config validation and model"`

---

### Task 4: Request target validation

**Files:**
- Create: `core/src/http/mod.rs`, `core/src/http/target.rs`
- Modify: `core/src/config/validate.rs` (route args use `validate_path`)

**Interfaces:**
- Produces:
  - `pub struct Target { pub path: String, pub query: Option<String> }`
  - `pub enum TargetError { NotOriginForm, Empty, DotSegment, EmptySegment, EncodedDot, EncodedSlash, EncodedBackslash, EncodedNul, BadPercentEncoding, Backslash, NonAscii, ControlOrSpace, Fragment }`
  - `pub fn parse_target(raw: &[u8]) -> Result<Target, TargetError>` — splits at the first `?`
  - `pub fn validate_path(path: &str) -> Result<(), TargetError>`

Rules: must start with `/` (else `NotOriginForm` — covers `http://…`, `example.com:80`, `*`). Reject bytes ≥ 0x80 (`NonAscii`), ≤ 0x20 or 0x7F (`ControlOrSpace`), `\` (`Backslash`), `#` (`Fragment`). Percent escapes need two hex digits (`BadPercentEncoding`); `%2E`/`%2e` → `EncodedDot`, `%2F` → `EncodedSlash`, `%5C` → `EncodedBackslash`, `%00` → `EncodedNul` (hex case-insensitive). In the path: `//` → `EmptySegment`; a segment equal to `.` or `..` → `DotSegment`. The query is checked for the byte-class rules only.

- [ ] **Step 1: Write the failing tests** — table test `rejects_hostile_targets` with one row per error, including `b"/a/%2e%2E/b"` → `EncodedDot`, `b"/a/%2E"` → `EncodedDot`, `b"/a/../b"` → `DotSegment`, `b"/a/."` → `DotSegment`, `b"/a//b"` → `EmptySegment`, `b"/%2f"` → `EncodedSlash`, `b"/%zz"` → `BadPercentEncoding`, `b"/%4"` → `BadPercentEncoding`, `b"http://x/"` → `NotOriginForm`, `b"*"` → `NotOriginForm`, `b"/a b"` → `ControlOrSpace`, `b"/\xc3\xa4"` → `NonAscii`; and `accepts_ordinary_targets`: `b"/"`, `b"/api/users?id=1&x=%20"` → `Target { path: "/api/users", query: Some("id=1&x=%20") }`, `b"/a.b/..c/c.."` (dots inside names are fine). Add a `proptest` (dev-dependency of core) `accepted_paths_never_contain_dot_segments_after_decoding`: for arbitrary byte strings, if `parse_target` succeeds then percent-decoding the path never yields a `.`/`..` segment, `//`, `\`, or NUL.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core http::target` → FAIL.
- [ ] **Step 3: Implement** `parse_target`, `validate_path`; use `validate_path` for `path` block args in config validation (error `InvalidValue`), with a config test for `path /a/../ { … }`.
- [ ] **Step 4: Run to verify they pass** — `cargo test -p durchlauferhitzer-core` → PASS.
- [ ] **Step 5: Commit** — `"Add strict request target validation"`

---

### Task 5: Incremental request-head parser

**Files:**
- Create: `core/src/http/request.rs`

**Interfaces:**
- Consumes: `parse_target`, `Target`, `TargetError` (Task 4); `Limits` (Task 3)
- Produces:
  - `pub enum Method { Get, Head, Other(String) }`
  - `pub enum Version { Http10, Http11 }`
  - `pub struct Header { pub name: String /* lowercase */, pub value: Vec<u8> /* OWS-trimmed */ }`
  - `pub enum BodyFraming { None, Length(u64), Chunked }`
  - `pub enum Persistence { KeepAlive, Close }`
  - `pub struct RequestHead { pub method: Method, pub target: Target, pub version: Version, pub host: Option<String> /* lowercase, raw incl. port */, pub headers: Vec<Header>, pub body: BodyFraming, pub persistence: Persistence }`
  - `pub enum HttpError { RequestLineTooLong, HeadTooLarge, TooManyHeaders, BadRequestLine, BadMethod, BadVersion, UnsupportedVersion, BadTarget(TargetError), BareLf, BadHeaderName, BadHeaderValue, ObsFold, MissingHost, DuplicateHost, BadHost, ContentLengthWithTransferEncoding, BadContentLength, DuplicateContentLength, UnsupportedTransferEncoding, TransferEncodingInHttp10 }`
  - `pub struct HeadLimits { pub max_request_line: usize, pub max_head: usize, pub max_headers: usize }` with `impl From<&Limits>`
  - `pub enum ParseStatus { Incomplete, Complete { head: RequestHead, consumed: usize } }`
  - `pub fn parse_head(buf: &[u8], limits: &HeadLimits) -> Result<ParseStatus, HttpError>`

`parse_head` is stateless: called with all bytes received so far for this request, returns `Incomplete` until the blank line arrives, and reports limit errors as soon as they are certain (no LF within `max_request_line + 2` bytes → `RequestLineTooLong` even while `Incomplete`; buffer past `max_head` without the blank line → `HeadTooLarge`). Request line: `token SP target SP version CRLF`, single spaces. Version `HTTP/1.1`/`HTTP/1.0`; other `HTTP/<digit>.<digit>` → `UnsupportedVersion`; anything else → `BadVersion`. No leading empty lines. Header line: `token ":" OWS value OWS CRLF`; value bytes must be HTAB, SP, VCHAR or ≥0x80; header line starting with SP/HTAB → `ObsFold`; any LF not preceded by CR → `BareLf`. Host: HTTP/1.1 requires exactly one; value is `reg-name` or `[IPv6]`, optional `:port` digits, else `BadHost`. HTTP/1.0: `Host` optional, `Transfer-Encoding` → `TransferEncodingInHttp10`, persistence always `Close`. HTTP/1.1: `Connection` header containing the token `close` (comma-separated, case-insensitive) → `Close`, else `KeepAlive`. `Transfer-Encoding` must be exactly one header whose value is `chunked` (case-insensitive) else `UnsupportedTransferEncoding`; `Content-Length` one header, 1+ digits, fits `u64`.

- [ ] **Step 1: Write the failing tests** — `parses_simple_get` (exact `RequestHead`, `consumed` = whole input); `incomplete_until_blank_line` (every proper prefix of a valid head → `Incomplete`); `leaves_pipelined_bytes_unconsumed` (two heads concatenated → `consumed` = first head's length); table test `rejects_malformed_heads` with one row per `HttpError` variant, including the Review Focus inputs: `b"GET / HTTP/1.1\nHost: a\n\n"` → `BareLf`, `b"GET / HTTP/1.1\r\nHost: a\r\nX: a\rb\r\n\r\n"` → `BadHeaderValue`, `b"…X: a\0b\r\n…"` → `BadHeaderValue`, `b"…Host : a\r\n…"` → `BadHeaderName`, `b"GET  / HTTP/1.1\r\n…"` → `BadRequestLine`, `b"GET / HTTP/2.0\r\n…"` → `UnsupportedVersion`, `Content-Length: 5` + `Transfer-Encoding: chunked` → `ContentLengthWithTransferEncoding`, two `Content-Length: 5` → `DuplicateContentLength`, `Transfer-Encoding: gzip, chunked` → `UnsupportedTransferEncoding`; `limits_are_enforced` (`max_request_line + 2` bytes with no LF → `RequestLineTooLong` while incomplete (`+ 1` could still be a full line plus its CR); `max_headers + 1` headers → `TooManyHeaders`; head over `max_head` → `HeadTooLarge`); `connection_close_and_http10_persistence`. Add a `proptest` `never_panics_on_arbitrary_bytes` (any `Vec<u8>` up to 4 KiB returns without panic).
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core http::request` → FAIL.
- [ ] **Step 3: Implement `parse_head`.**
- [ ] **Step 4: Run to verify they pass** — same → PASS; clippy clean.
- [ ] **Step 5: Commit** — `"Add strict incremental request-head parser"`

---

### Task 6: Router

**Files:**
- Create: `core/src/routing.rs`

**Interfaces:**
- Consumes: `Config`, `VirtualHost`, `Route` (Task 3)
- Produces:
  - `pub struct Router` built by `pub fn Router::new(hosts: &[VirtualHost]) -> Router` (own copies or indices; no lifetimes in `Core`)
  - `pub enum RouteMiss { UnknownHost, NoMatchingPath }`
  - `pub fn Router::route(&self, host: Option<&str>, path: &str) -> Result<&Route, RouteMiss>`

Host normalization: lowercase; strip `:port` (for `[v6]:port`, keep the bracketed part and strip the port); no other normalization (a trailing dot stays and therefore misses). Path matching: prefix `P` matches `X` iff `X == P`, or `X` starts with `P` and (`P` ends with `/` or the next byte of `X` is `/`). Longest matching prefix wins. `None` host → `UnknownHost`.

- [ ] **Step 1: Write the failing tests** — `longest_prefix_wins` (`/` vs `/api/` for `/api/x`), `prefix_matches_on_segment_boundaries` (`/api/` does not match `/apix` nor `/api`; prefix `/api` matches `/api`, `/api/x`, not `/apix`), and the Review Focus host table: `EXAMPLE.test:8080` → match, `[::1]:8080` with host name `[::1]` configured → match, `example.test.` → `UnknownHost`, `None` → `UnknownHost`.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core routing` → FAIL.
- [ ] **Step 3: Implement `Router`.** Note: Task 3 must accept `[::1]` as a host name (IP literal) — if it doesn't yet, add that config test and fix here.
- [ ] **Step 4: Run to verify they pass** → PASS.
- [ ] **Step 5: Commit** — `"Add virtual host and path-prefix router"`

---

### Task 7: Status codes and fixed responses

**Files:**
- Create: `core/src/http/status.rs`, `core/src/http/response.rs`

**Interfaces:**
- Consumes: `HttpError` (Task 5), `Persistence` (Task 5)
- Produces:
  - `pub enum Status { BadRequest, NotFound, RequestTimeout, UriTooLong, RequestHeaderFieldsTooLarge, NotImplemented, ServiceUnavailable, HttpVersionNotSupported }` with `pub fn code(self) -> u16` and `pub fn reason(self) -> &'static str`
  - `impl HttpError { pub fn status(&self) -> Status }` — `RequestLineTooLong` → 414, `HeadTooLarge`/`TooManyHeaders` → 431, `UnsupportedVersion` → 505, all others → 400
  - `pub enum BodyMode { Full, HeadOnly }`
  - `pub fn render_fixed(status: Status, persistence: Persistence, body: BodyMode, out: &mut Vec<u8>)`

Exact bytes (the spec's copy; body is `"<code> <reason>\n"`):

```
HTTP/1.1 404 Not Found\r\n
Content-Type: text/plain; charset=utf-8\r\n
Content-Length: 14\r\n
X-Content-Type-Options: nosniff\r\n
Connection: close\r\n          <- only for Persistence::Close
\r\n
404 Not Found\n                <- only for BodyMode::Full
```

- [ ] **Step 1: Write the failing tests** — `renders_404_keep_alive` (exact bytes as above without `Connection`), `renders_400_close_head_only` (exact bytes, `Content-Length: 16` present, no body), `status_codes_and_reasons` (every variant: 400 Bad Request, 404 Not Found, 408 Request Timeout, 414 URI Too Long, 431 Request Header Fields Too Large, 501 Not Implemented, 503 Service Unavailable, 505 HTTP Version Not Supported), `http_errors_map_to_statuses`.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core http::` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass** → PASS.
- [ ] **Step 5: Commit** — `"Add status codes and fixed responses"`

---

### Task 8a: Core interface and simulation harness

Split from the original Task 8 so the driver↔core interface and the simulation are reviewed before request handling builds on them. No HTTP parsing yet; no deadlines (Task 9).

**Files:**
- Create: `core/src/time.rs`, `core/src/core.rs`, `core/src/conn.rs`, `core/src/log.rs`, `sim/Cargo.toml`, `sim/src/lib.rs`, `sim/tests/connections.rs`
- Modify: `Cargo.toml` (add `sim` member), `core/src/lib.rs`

**Interfaces:**
- Consumes: `load`, `Config`, `Limits` (3); `Method`, `HttpError` (5); `RouteMiss` (6); `Status` (7)
- Produces (core, re-exported at crate root):
  - `pub struct Instant(u64)` — nanoseconds on an arbitrary monotonic epoch; `Instant::from_nanos(u64)`, `fn saturating_add(self, Duration) -> Instant`, `Ord`
  - `pub struct ConnId(pub u64)` — allocated by the driver, never reused
  - `pub enum ConnError { Reset, Other }`
  - `pub enum Event { Accepted { conn: ConnId, peer: IpAddr }, Received { conn: ConnId, len: usize }, ReadClosed { conn: ConnId }, Sent { conn: ConnId, len: usize }, Failed { conn: ConnId, error: ConnError }, TimerFired }`
  - `pub enum Interest { None, Read, Write }`
  - `pub enum Action { SetInterest { conn: ConnId, interest: Interest }, ShutdownWrite { conn: ConnId }, Close { conn: ConnId }, Log(Log) }`
  - `pub enum Log { Response { conn: ConnId, peer: IpAddr, status: Status, request: Option<RequestSummary> }, Rejected { conn: ConnId, peer: IpAddr, reason: RejectReason }, Reset { conn: ConnId, peer: IpAddr } }`, `pub struct RequestSummary { pub method: Method, pub host: Option<String>, pub path: String }`, `pub enum RejectReason { Http(HttpError), Route(RouteMiss), Timeout(TimeoutKind), Limit(LimitKind) }`, `pub enum TimeoutKind { Header, Send }`, `pub enum LimitKind { Connections, ConnectionsPerIp }`
  - `pub struct Core` with
    - `pub fn new(config: Config) -> Core`
    - `pub fn handle(&mut self, now: Instant, event: Event, actions: &mut Vec<Action>)`
    - `pub fn recv_buffer(&mut self, conn: ConnId) -> &mut [u8]` — writable spare space; empty when the connection is unknown or not reading
    - `pub fn send_buffer(&self, conn: ConnId) -> &[u8]` — bytes waiting to be written; empty when none
    - `pub fn next_deadline(&self) -> Option<Instant>` — always `None` in this task
- Produces (sim):
  - `pub struct Sim` with `pub fn new(config: &str) -> Sim`, `pub fn connect(&mut self, peer: IpAddr) -> ConnId`, `pub fn send(&mut self, conn: ConnId, bytes: &[u8])`, `pub fn send_eof(&mut self, conn: ConnId)`, `pub fn reset(&mut self, conn: ConnId)`, `pub fn set_read_chunk(&mut self, conn: ConnId, max: usize)` (max bytes delivered per `Received`), `pub fn set_client_window(&mut self, conn: ConnId, window: Window)`, `pub fn run_until_idle(&mut self)`, `pub fn advance(&mut self, by: Duration)`, `pub fn now(&self) -> Instant`, `pub fn take_received(&mut self, conn: ConnId) -> Vec<u8>`, `pub fn queued_to_server(&self, conn: ConnId) -> usize`, `pub fn interest(&self, conn: ConnId) -> Interest`, `pub fn state(&self, conn: ConnId) -> SimConnState`, `pub fn logs(&self) -> &[Log]`, `pub fn trace(&self) -> &str`
  - `pub enum Window { Unlimited, Bytes(usize), Stalled }` — how many bytes the simulated client accepts per `Sent`
  - `pub enum SimConnState { Open, WriteShutdown, Closed }`

Core behaviour in this task: `Accepted` allocates the receive buffer (capacity `max(max_head, max_request_line + 2)` — see the Task 5 ruling; the parser may need `max_request_line + 2` bytes for a verdict) and a send buffer of 1 KiB, creates `ConnState::ReadingHead { buffered: usize }` (`conn.rs`; Tasks 8b and 9 add variants and fields), and emits `SetInterest(Read)`. `Received` adds to `buffered`; when the buffer is full it emits `SetInterest(None)` (backpressure). `ReadClosed` → `Close`, no log. `Failed` → `Close` + `Log::Reset`. Events for unknown connections are ignored. `TimerFired` does nothing. Both buffers are freed on close.

The sim pumps until no progress: for each connection with `Interest::Read` and queued client bytes, copy `min(recv_buffer.len(), read_chunk, queued)` and emit `Received`; with queued EOF and nothing left, emit `ReadClosed`; for `Interest::Write`, move `min(send_buffer.len(), window)` into the client's received bytes and emit `Sent`. `advance` fires `TimerFired` at each `next_deadline()` in order up to the target time, pumping after each, then sets the clock to the target. The sim records every event and action in `trace()` and panics (test failure, with `trace()` in the message) on invariant violations: an action for an unknown or closed connection, a second `Close`, any action after `Close`, `recv_buffer`/`send_buffer` longer than their capacities, `send_buffer` non-empty while interest stays `Read` after a pump.

- [ ] **Step 1: Write the failing tests** in `sim/tests/connections.rs` (minimal valid config: `listen 127.0.0.1:8080; host a.test { path / { static /srv; } }`):
  - `accepted_connection_reads` → `state == Open`, `interest == Interest::Read`, no logs
  - `client_reset_closes_and_logs` → `reset` → `Closed`, `logs() == [Log::Reset { conn, peer }]`
  - `client_eof_closes_silently` → `send_eof` → `Closed`, no logs
  - `receive_buffer_applies_backpressure` → with `limits { max_request_line 64; max_head 64; }`, send 100 bytes: exactly 66 are delivered (`queued_to_server == 34`), `interest == Interest::None`, still `Open`
  - `read_chunk_limits_each_delivery` → `set_read_chunk(c, 1)`, send 10 bytes → `trace()` shows 10 `Received` events of length 1
  - `connections_are_independent` → reset one of two connections; the other stays `Open` and reading
  - `advance_moves_the_clock` → `advance(5s)` → `now()` is 5 s later
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-sim` → FAIL.
- [ ] **Step 3: Implement** `time.rs`, `log.rs`, `conn.rs`, `core.rs`, then the `Sim`.
- [ ] **Step 4: Run to verify they pass** — `cargo test --workspace` → PASS; clippy clean; `scripts/check-product-deps.sh` passes (sim's crates do not appear in core's tree).
- [ ] **Step 5: Commit** — `"Add core interface and simulation harness"`

---

### Task 8b: Request handling

**Files:**
- Modify: `core/src/core.rs`, `core/src/conn.rs`
- Create: `sim/tests/routing.rs`

**Interfaces:**
- Consumes: Task 8a's `Core`, `Sim`, `Log`; `parse_head`, `HeadLimits` (5); `Router` (6); `render_fixed`, `Status`, `BodyMode` (7)
- Produces: no new public API (behaviour only).

Connection states (`conn.rs`), each variant carrying only its own data: `ReadingHead { buffered: usize, scanned: usize }`, `Idle` (keep-alive, nothing buffered), `Responding { persistence: Persistence, unread_body: UnreadBody }` (`enum UnreadBody { None, Pending }`), `Lingering`. Transitions: complete head → route → render → `Responding`; response fully sent → if `Close` or body unread → `ShutdownWrite`, `Lingering`; else buffered bytes present → `ReadingHead` and parse immediately; else `Idle`. First byte in `Idle` → `ReadingHead`. `Lingering` reads and discards input until `ReadClosed` → `Close`. `Failed`/`ReadClosed` outside `Lingering` → `Close` (+ `Log::Reset` for `Failed`). Matched routes answer `501 Not Implemented` with `Persistence::Close` in this stage. A request with `BodyFraming` other than `None` marks `UnreadBody::Pending` (forces close). `HEAD` gets `BodyMode::HeadOnly`. Parse errors answer `err.status()` with `Persistence::Close`; route misses answer 404 keeping the request's persistence. Interest is `Write` while `send_buffer` is non-empty, `Read` otherwise (and `None` when the receive buffer is full). Bytes after a complete head stay buffered for the next request (move them to the front of the buffer).

To avoid re-scanning cost (Task 5 note: `parse_head` rescans from byte 0), `ReadingHead.scanned` remembers how far the buffer has been searched for a new LF; `parse_head` is called again only when a `Received` adds an LF beyond `scanned`, or when the buffer is full (so a limit error can fire).

- [ ] **Step 1: Write the failing tests** in `sim/tests/routing.rs` using this config:

```
listen 127.0.0.1:8080;
host example.test www.example.test {
  path / { static /srv/www; }
  path /api/ { proxy 127.0.0.1:3000; strip_prefix /api; max_request_body 1MiB; max_response_body unlimited; }
}
```

  - `unknown_host_gets_404_and_keeps_connection` → received == `render_fixed(NotFound, KeepAlive, Full)`, `state == Open`, `Log::Response { status: NotFound, .. }` with the request summary
  - `matched_route_gets_501_and_closes` → `GET /api/x` with `Host: example.test` → 501 bytes with `Connection: close`, then `WriteShutdown`; after `send_eof` + `run_until_idle` → `Closed`
  - `malformed_request_gets_400_and_closes` → `b"GET / HTTP/1.1\nHost: a\n\n"` → 400 with close, `Log::Rejected { reason: Http(BareLf), .. }`
  - `head_request_gets_no_body`
  - `keep_alive_serves_sequential_requests` → two requests one after the other on the same connection → two 404s, still `Open`
  - `pipelined_requests_answered_in_order` → two requests in one `send` → two 404s in order
  - `unread_body_closes_after_response` → `GET /nowhere` with `Content-Length: 10` → 404 with `Connection: close`, `WriteShutdown`, further bytes discarded, `Closed` after `send_eof`
  - `slow_client_window_still_gets_whole_response` → `Window::Bytes(3)` → full 404 bytes arrive intact
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-sim --test routing` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --workspace` → PASS; clippy clean.
- [ ] **Step 5: Commit** — `"Handle requests: parse, route, answer"`

---

### Task 9: Timeouts, connection caps and lingering close

**Files:**
- Modify: `core/src/core.rs`, `core/src/conn.rs`
- Create: `sim/tests/limits.rs`

**Interfaces:**
- Consumes: everything from Tasks 8a/8b; `Limits` fields `header_timeout`, `keepalive_timeout`, `send_idle_timeout`, `max_connections`, `max_connections_per_ip`
- Produces: no new public API (behavior only).

This task adds the deadlines: `ReadingHead` gets a total header deadline (now + `header_timeout`, set when the first byte of a request arrives or at accept), `Idle` a keep-alive deadline (now + `keepalive_timeout`), `Responding` a send idle deadline (now + `send_idle_timeout`, reset on every `Sent` with len > 0), `Lingering` a deadline of now + 2 s (constant `LINGER_TIMEOUT`); `next_deadline` returns the earliest. Behavior: header deadline expiry with `bytes_seen > 0` → 408 + close (`Rejected { reason: Timeout(Header) }`); with `bytes_seen == 0` → silent `Close`. `Idle` expiry → silent `Close`. Send idle expiry → `Close` without further writes (`Rejected { reason: Timeout(Send) }`). On `Accepted` when open connections ≥ `max_connections` or this IP's open connections ≥ `max_connections_per_ip`: answer 503 with close and linger (`Rejected { reason: Limit(..) }`); these refused connections do not count toward either limit. Counts drop when a connection is closed.

- [ ] **Step 1: Write the failing tests** (config with `limits { header_timeout 10s; keepalive_timeout 60s; send_idle_timeout 30s; max_connections 4; max_connections_per_ip 2; }`):
  - `trickling_head_times_out_at_total_deadline` → send one byte every 2 s; at 10 s total → 408 + close (trickling does not extend it)
  - `silent_idle_connection_is_closed_without_response` → connect, advance 10 s → `Closed`, nothing received
  - `keepalive_idle_closes_after_60s` (59 s still `Open`, 60 s `Closed`)
  - `stalled_reader_is_closed_after_send_idle_timeout` → `Window::Stalled`, request → advance 30 s → `Closed`; with `Window::Bytes(1)` the connection stays open as long as bytes keep moving
  - `per_ip_cap_answers_503` → third connection from the same IP gets 503; a connection from another IP is served normally; after closing one of the first two, a new one from that IP is served
  - `global_cap_answers_503`
  - Review Focus: `unread_body_is_drained_before_close` → `GET /nowhere` with `Content-Length: 100000` and the body streaming in 1 KiB pieces: the 404 arrives completely, then `WriteShutdown`; further body bytes are accepted and discarded (no `Closed` until `send_eof` or 2 s pass)
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-sim --test limits` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass** — `cargo test --workspace` → PASS.
- [ ] **Step 5: Commit** — `"Enforce timeouts, connection caps and lingering close"`

---

### Task 10: Fault injection and randomized simulation

**Files:**
- Create: `sim/tests/faults.rs`
- Modify: `sim/Cargo.toml` (dev-dependency `fastrand`), `sim/src/lib.rs` if a helper is missing

**Interfaces:**
- Consumes: `Sim` API (Task 8a)
- Produces: no new public API.

- [ ] **Step 1: Write the tests**
  - `every_split_point_gives_identical_responses` — for each split index of a valid request (and of an invalid one), deliver the two halves in separate pumps; received bytes equal the single-read result
  - `byte_by_byte_delivery` — `set_read_chunk(c, 1)` for a keep-alive pair of requests
  - `pipelined_requests_in_one_read` — two requests in one `send` → both responses, in order, same bytes as sequentially
  - `pipelined_bytes_beyond_head_limit` — first request followed by more than `max_head` bytes of a second → first answered normally, second rejected with 431
  - `random_fault_schedules` — 500 seeds (env `SIM_SEED` runs a single seed): each seed builds a random mix of valid/invalid requests, random read chunk sizes, random client windows including stalls, random resets and EOFs, and random clock advances; asserts the sim invariants hold and every connection ends `Closed` after advancing 5 minutes with all clients sending EOF. On failure the panic message contains `SIM_SEED=<seed>` and the trace.
- [ ] **Step 2: Run them** — `cargo test -p durchlauferhitzer-sim --test faults`. These tests target behavior Tasks 8–9 should already have; any that pass immediately are fine, but each failure is a red test: fix the core (not the test) and keep the failing seed as a named regression test.
- [ ] **Step 3: Fix any failures found** in `core/src/*.rs`.
- [ ] **Step 4: Run everything** — `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && scripts/check-product-deps.sh` → all PASS.
- [ ] **Step 5: Commit** — `"Add fault-injection and randomized simulation tests"`
