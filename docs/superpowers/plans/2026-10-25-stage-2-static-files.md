# Stage 2: Static Files Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** Static routes serve files from their root through the sans-IO core: the core decides what to open, read ahead and send; a simulated file system in the sim stands in for the disk, including slow reads, symlinks and files that change mid-transfer.

**Architecture:** The core gains file-ids and three file actions (open, read ahead a chunk, close) plus file events (opened, failed, chunk ready, file bytes sent). File bytes never enter the core: like `send_buffer`, the core exposes the current file segment for the driver to `sendfile`. The sim gets an in-memory file system and performs "sendfile" by copying from it. The real epoll driver (stage 4) implements the same contract with `openat2`, a readahead pool and `sendfile`.

**Tech Stack:** Rust 2024, std only in core; sim test-only (fastrand already present).

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` — sections "Static routes", "Drivers", "Streaming and flow control". The amendments below are applied to the spec together with this plan.

## Spec amendments (decided while planning; listed for the owner's review)

1. **Static routes strip their own prefix** (alias semantics): on `path /assets { static /srv/assets; }`, `/assets/a.css` serves `/srv/assets/a.css`.
2. **A directory requested without a trailing `/`** gets `301 Moved Permanently` with `Location: <path>/` (plus the original query). With the `/`, its `index.html` is served if present, else 404.
3. **Permission denied, symlinks, dotfiles, missing files, non-regular files** (devices, sockets, FIFOs) → all 404, so the response does not reveal which.
4. **405** responses carry `Allow: GET, HEAD`.
5. **Error responses to HEAD requests** carry no body (any request whose method was parsed).
6. **Waiting on the disk** counts against `send_idle_timeout` (no separate disk timeout): a connection whose open or readahead does not complete in time is closed and logged.
7. **A file that changes during a transfer** is handled by a per-route setting `on_file_change abort | ignore;` (optional, default `abort`). The driver reports the file's current stamp (size + modification time) with every chunk. `abort` (the safe default): any change to size or modification time since open cuts the connection after the headers were sent and is logged. `ignore`: keep sending exactly the size seen at open as long as the file has not shrunk. A file that shrank aborts under both settings, since its promised length can no longer be delivered.
8. **Content types:** `html htm` text/html; charset=utf-8, `css` text/css; charset=utf-8, `js mjs` text/javascript; charset=utf-8, `json` application/json, `txt` text/plain; charset=utf-8, `xml` application/xml, `svg` image/svg+xml, `png` image/png, `jpg jpeg` image/jpeg, `gif` image/gif, `webp` image/webp, `avif` image/avif, `ico` image/x-icon, `woff` font/woff, `woff2` font/woff2, `pdf` application/pdf, `wasm` application/wasm, `mp4` video/mp4, `webm` video/webm; anything else application/octet-stream. Extension match is ASCII case-insensitive on the last `.` of the file name.

## Global Constraints

- Everything in stage 1's Global Constraints still holds (zero product dependencies in core, no `unsafe`, denied clippy lints in core, no `_ =>` on enums, enums over bools, strict red/green TDD, bounded memory).
- File bytes never pass through core memory; only the response head goes through `send_buffer`.
- Every opened file is closed exactly once on every path (normal end, error, reset, timeout); the sim checks this.
- At most one open file per connection; file reads are requested one chunk at a time (`FILE_CHUNK` = 256 KiB) and never ahead of what the client has taken.
- The decoded request path is never logged or echoed; logs keep the raw (validated, ASCII) path.

## Review Focus

1. A file that shrinks between open and send — the client must get a cut connection, never a short body that looks complete. Test in Task 4.
2. A slow disk and a stalled client at the same time on different connections — other connections keep being served. Test in Task 4.
3. Dotfiles reached by a plain path (`/.env`, `/a/.git/config`) → 404 (encoded dots are already rejected with 400 upstream). Test in Tasks 1 and 3.
4. A connection reset while its open or readahead is in flight — the late `FileOpened`/`ChunkReady` must be harmless and the file closed. Test in Task 4.
5. HEAD on a large file — headers with the full `Content-Length`, no open of more than the metadata, file closed. Test in Task 3.

---

## File Structure

```
core/src/http/static_path.rs   decode a validated path to relative segments; dotfile rule
core/src/http/mime.rs          content-type table
core/src/file.rs               FileId, FileKind, FileMeta, FileError, RootId, FileSegment
core/src/core.rs               file actions/events, static route handling (+ contract docs)
core/src/conn.rs               states for serving a file
core/src/http/status.rs        + Ok, MovedPermanently, MethodNotAllowed, ContentTooLarge
core/src/http/response.rs      + render_file_head, render_redirect; Allow header on 405
sim/src/fs.rs                  in-memory file system with fault injection
sim/src/lib.rs                 performs file actions; file invariants
sim/tests/static_files.rs      end-to-end static serving tests
sim/tests/faults.rs            random runs extended with static routes
```

---

### Task 1: Decoded relative paths and content types

**Files:**
- Create: `core/src/http/static_path.rs`, `core/src/http/mime.rs`
- Modify: `core/src/http/mod.rs`

**Interfaces:**
- Consumes: `validate_path`-checked paths (stage 1 Task 4); `strip_segments` semantics (Task 11)
- Produces:
  - `pub enum StaticPathError { DotSegment }`
  - `pub fn relative_segments(path: &str, skip: usize) -> Result<Vec<Vec<u8>>, StaticPathError>` — skips the route's `skip` segments, percent-decodes each remaining segment exactly once (bytes, may be non-UTF-8), drops the empty trailing segment, rejects any decoded segment starting with `.`
  - `pub enum TrailingSlash { Present, Absent }` and `pub fn trailing_slash(path: &str) -> TrailingSlash`
  - `pub fn content_type(file_name: &[u8]) -> &'static str` — the table in spec amendment 8

- [ ] **Step 1: Write the failing tests** — `relative_segments("/assets/a%20b/c.css", 1) == Ok(vec![b"a b".to_vec(), b"c.css".to_vec()])`; `("/", 0) == Ok(vec![])`; `("/a/", 0) == Ok(vec![b"a".to_vec()])`; `("/%C3%A4", 0) == Ok(vec![vec![0xC3, 0xA4]])`; `("/.hidden", 0)`, `("/a/.git/config", 0)`, `("/x/.y", 1)` → `Err(DotSegment)`; `trailing_slash("/a/") == Present`, `("/a") == Absent`, `("/") == Present`; content types: `b"index.HTML"` → html type, `b"a.tar.gz"` → octet-stream, `b"noext"` → octet-stream, `b".x"` unreachable but → octet-stream, every table row.
- [ ] **Step 2: Run to verify they fail** — `cargo test -p durchlauferhitzer-core http::static_path http::mime` → FAIL.
- [ ] **Step 3: Implement** both modules.
- [ ] **Step 4: Run to verify they pass**; full pre-commit checks.
- [ ] **Step 5: Commit** — `"Decode static paths to relative segments; content types"`

---

### Task 2: File interface and simulated file system

Like stage 1's Task 8a: the contract and the sim first, with only the simplest behaviour (GET of an existing regular file) so the interface is exercised.

**Files:**
- Create: `core/src/file.rs`, `sim/src/fs.rs`, `sim/tests/static_files.rs`
- Modify: `core/src/core.rs`, `core/src/conn.rs`, `core/src/lib.rs`, `core/src/http/status.rs`, `core/src/http/response.rs`, `sim/src/lib.rs`

**Interfaces:**
- Consumes: Task 1; `Router`, `RouteAction::Static { root }` (stage 1)
- Produces (core, re-exported at crate root):
  - `pub struct FileId(pub u64)` — allocated by the core, never reused
  - `pub struct RootId(pub u32)` — index into `Core::static_roots()`
  - `pub fn Core::static_roots(&self) -> &[PathBuf]` — every distinct static root in config order; the driver opens them once at startup
  - `pub enum FileKind { Regular, Directory, Other }`, `pub struct FileStamp { pub size: u64, pub modified_ns: u64 }`, `pub struct FileMeta { pub kind: FileKind, pub stamp: FileStamp }`
  - `pub enum FileError { NotFound, Symlink, PermissionDenied, Truncated, Changed, Other }`
  - `Action::OpenFile { file: FileId, root: RootId, path: Vec<Vec<u8>> }` (relative segments; the driver resolves beneath the root without following symlinks and without leaving it), `Action::Readahead { file: FileId, offset: u64, len: u64 }`, `Action::CloseFile { file: FileId }`
  - `Event::FileOpened { file: FileId, meta: FileMeta }`, `Event::FileFailed { file: FileId, error: FileError }`, `Event::ChunkReady { file: FileId, stamp: FileStamp }` (the file's stamp when the chunk was read), `Event::FileSent { conn: ConnId, len: u64 }`
  - `pub struct FileSegment { pub file: FileId, pub offset: u64, pub len: u64 }` and `pub fn Core::send_file(&self, conn: ConnId) -> Option<FileSegment>` — the file range the driver may `sendfile` now; only after `send_buffer` is empty
  - `Status::Ok` (200) and `pub fn render_file_head(content_type: &str, len: u64, persistence: Persistence, out: &mut Vec<u8>)` — `HTTP/1.1 200 OK`, `Content-Type`, `Content-Length`, `X-Content-Type-Options: nosniff`, `Connection: close` only for `Close`
- Produces (sim):
  - `pub fn Sim::add_file(&mut self, root: &str, path: &str, bytes: &[u8])`, `add_dir`, `add_symlink(root, path, target)`, `deny(root, path)` (permission denied), `set_disk_delay(&mut self, d: Duration)` (each open/readahead completes after `d` of virtual time), `shrink_on_send(root, path, at: u64)` (sendfile hits EOF at `at`), `change_during_send(root, path, after_chunks: usize, new: FileStamp)` (later chunks report a different stamp), `pub fn open_files(&self) -> usize`

Contract additions (core module docs): file ids are the core's; every `OpenFile` is answered by exactly one `FileOpened` or `FileFailed`, every `Readahead` by one `ChunkReady` or `FileFailed`, even after the connection closed (the core then just emits `CloseFile` for a late `FileOpened`); the driver writes `send_buffer` before `send_file`; interest is `Write` while either has bytes; `FileSent` reports bytes the socket accepted from the segment; a short `sendfile` at EOF before the segment ends is reported as `FileFailed { error: Truncated }`; after `CloseFile` the driver delivers no more events for that file. Behaviour in this task: GET on a static route whose path resolves to a regular file → `OpenFile`, then on `FileOpened{Regular}` render the head, then `Readahead` of the first chunk, then on `ChunkReady` expose the segment; continue chunk by chunk; after the last byte `CloseFile` and return to `Idle`/close per persistence. Everything else on static routes still answers 501 (Task 3 replaces that).

Sim invariants added: an action for an unknown or closed file; a second `CloseFile`; `send_file` returning a segment while `send_buffer` is non-empty or before its `ChunkReady`; files left open when a connection closed and its pending file events were delivered; a readahead beyond the size seen at open.

- [ ] **Step 1: Write the failing tests** in `sim/tests/static_files.rs` (config: `path / { static /srv/www; }` and `path /assets { static /srv/assets; }` on host `example.test`):
  - `serves_small_file` → exact bytes `render_file_head("text/html; charset=utf-8", 5, KeepAlive)` + `hello`, connection `Open`, `open_files() == 0`
  - `serves_file_larger_than_one_chunk` → a 600 KiB file arrives intact; the trace shows 3 `Readahead`s, each after the previous chunk was fully sent
  - `alias_semantics_strip_route_prefix` → `/assets/a.css` reads `a.css` from `/srv/assets`
  - `slow_disk_still_delivers` → `set_disk_delay(50ms)`, file arrives intact after advancing time
  - `static_roots_are_listed` → `Core::static_roots() == ["/srv/www", "/srv/assets"]` (via a sim accessor)
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** (core file types and docs first, then core behaviour, then the sim file system).
- [ ] **Step 4: Run everything** — `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh` → PASS.
- [ ] **Step 5: Commit** — `"File interface and simulated file system"`

---

### Task 3: Static route rules

**Files:**
- Modify: `core/src/core.rs`, `core/src/conn.rs`, `core/src/http/status.rs`, `core/src/http/response.rs`
- Test: `sim/tests/static_files.rs`

**Interfaces:**
- Consumes: Tasks 1–2
- Produces: `Status::{MovedPermanently, MethodNotAllowed, ContentTooLarge}` (301, 405, 413); `pub fn render_redirect(location: &str, persistence: Persistence, body: BodyMode, out: &mut Vec<u8>)` (fixed body `"301 Moved Permanently\n"`, `Location` header); `render_fixed` adds `Allow: GET, HEAD` for 405.

Rules (spec "Static routes" + amendments 2–5): methods other than GET/HEAD → 405 with `Allow`; a request body (any `BodyFraming` other than `None`) → 413 + close; `DotSegment` → 404; `FileFailed{NotFound|Symlink|PermissionDenied|Other}` → 404; `FileOpened{Other}` → `CloseFile` + 404; `FileOpened{Directory}` → `CloseFile`, then with `TrailingSlash::Absent` → 301 to raw path + `/` (+ `?query`), with `Present` → `OpenFile` of the same segments + `index.html` (its `Directory`/`Other` → 404); HEAD → head only (`Content-Length` = file size), no `Readahead`, `CloseFile` right after the head is queued; error responses to HEAD use `BodyMode::HeadOnly` (amendment 5, also for non-static responses); all responses logged once via `Log::Response` with the raw path.

- [ ] **Step 1: Write the failing tests** — `post_gets_405_with_allow`, `body_gets_413_and_closes`, `missing_file_404`, `symlink_404`, `permission_denied_404`, `dotfile_404` (`/.env`, `/a/.git/config`), `fifo_or_device_404` (sim `add_special`), `directory_without_slash_redirects` (exact bytes incl. `Location: /docs/?x=1` for `/docs?x=1`), `directory_with_slash_serves_index`, `directory_without_index_404`, `head_on_large_file_sends_head_only` (Review Focus 5; no `Readahead` in trace, `open_files() == 0`), `head_error_response_has_no_body` (HEAD to an unknown host → 404 head only).
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** (pre-commit checks) → PASS.
- [ ] **Step 5: Commit** — `"Static route rules: methods, bodies, directories, 404s, HEAD"`

---

### Task 4: Failures mid-transfer, slow disks and timeouts

**Files:**
- Modify: `core/src/core.rs`, `core/src/conn.rs`, `core/src/log.rs`, `core/src/config/model.rs`, `core/src/config/validate/` (new static-route key `on_file_change`: one value, `abort` or `ignore`; only on static routes; default `abort`; model `pub enum OnFileChange { Abort, Ignore }` in `RouteAction::Static { root, on_file_change }`)
- Test: `sim/tests/static_files.rs`, config tests

**Interfaces:**
- Consumes: Tasks 2–3
- Produces: `Log::FileAborted { conn: ConnId, peer: IpAddr, error: FileError }`; `TimeoutKind::Disk` (logged when `send_idle_timeout` expires while waiting on `FileOpened`/`ChunkReady`).

Rules: `ChunkReady` whose stamp differs from the one at open → with `on_file_change abort` (default) treat as a mid-transfer failure with `FileError::Changed`; with `ignore`, continue unless the new size is smaller than the size at open (then `Truncated`). `FileFailed` after the head was queued (any error incl. `Truncated`) → `CloseFile` (if open), `Log::FileAborted`, `Close` without lingering more bytes (the client sees a cut body); while waiting on the disk, `send_idle_timeout` runs and expiry → `Rejected{Timeout(Disk)}` + `CloseFile` when the file is open + `Close` (the late `FileOpened`/`ChunkReady` for that file then yields only `CloseFile` or nothing); reset/EOF/send-timeout while a file is open → `CloseFile` exactly once.

- [ ] **Step 1: Write the failing tests** — `file_shrinking_mid_transfer_cuts_connection` (Review Focus 1: received bytes < head's Content-Length, `Closed`, `Log::FileAborted{Truncated}`), `slow_disk_does_not_block_other_connections` (Review Focus 2: conn A on a 10 s disk delay, conn B gets a 404 and a small file meanwhile), `disk_slower_than_send_idle_timeout_closes` (`Rejected{Timeout(Disk)}`, file closed after the late events), `reset_during_open_closes_file_later` (Review Focus 4), `reset_during_readahead_closes_file`, `stalled_client_with_open_file_times_out_and_closes_file`, `changed_file_aborts_by_default` (`Log::FileAborted{Changed}`), `ignore_setting_keeps_sending_grown_file` (exactly the size at open is sent, connection stays open), `ignore_setting_still_aborts_on_shrink`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** (pre-commit checks) → PASS.
- [ ] **Step 5: Commit** — `"Abort cleanly on file failures, slow disks and timeouts"`

---

### Task 5: Static files in the randomized simulation

**Files:**
- Modify: `sim/tests/faults.rs`

**Interfaces:**
- Consumes: the sim file-system API (Task 2)

- [ ] **Step 1: Extend the tests** — each seed builds a small random tree (files of 0 B to 1 MiB, directories with and without `index.html`, symlinks, a denied file, a special file), random disk delays, random `shrink_on_send` and `change_during_send`, routes with both `on_file_change` settings; request templates add static GET/HEAD for existing, missing, directory and dotfile paths. Response checking accepts 200 heads (exact `render_file_head` for the file's type and size) followed by exactly `size` body bytes or a cut connection with `FileAborted`; 301 bytes exact. End-of-run checks add `open_files() == 0`. Coverage adds 200, 301, 405, 413, `FileAborted`, `Timeout(Disk)`.
- [ ] **Step 2: Run** — any failure is a core bug: fix it with a named regression test (pinned seed).
- [ ] **Step 3: Run everything** (pre-commit checks; keep the run under ~10 s debug) → PASS.
- [ ] **Step 4: Commit** — `"Randomized simulation covers static files"`
