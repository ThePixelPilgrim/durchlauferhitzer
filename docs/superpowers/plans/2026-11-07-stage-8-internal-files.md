# Stage 8: Backend-directed file serving and range requests — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** A proxy route with `internal_files` sends a file from a folder on this machine when its backend answers `200` with `X-Sendfile` or `X-Accel-Redirect`; every file response (static or backend-directed) serves single byte ranges.

**Architecture:**
- **Ranges first, on static routes.** A request's `Range` and `If-Range` are read into a small fixed `RangeRequest` and judged after the 304 check, on the stamp of the opened descriptor. A `206` is today's `SendingFile` started at the range's first byte and ended after its last. The driver already `sendfile`s and `readahead`s at explicit offsets.
- **Roots.** An `internal_files` root joins `Core::static_roots()`. The driver's startup open, the `openat2` jail, Landlock and the descriptor budget then cover it unchanged.
- **Header hygiene, both directions,** in `http/forward.rs`: the four file headers are stripped from every client request, the Rack hints are set on `internal_files` routes, and file headers are stripped from every backend response.
- **The trigger** sits in `Core::relay`, before any relaying check. It validates the path (a pure module) and renders the backend's kept header lines into the exchange's head buffer. It parks or closes the upstream without reading the body, then opens the file like a static route. After the open, the static file answer (`304` / `200` / `206` / `416` / `404`) is built around the kept lines.
- **No driver change** but the log format: no new system call or argument (see Task 1).

**Tech Stack:** Rust (stable, pinned), `libc` only. Test-only: the sim (`fastrand`), `bench/` (std).

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, binding. This plan covers:
- FR-STATIC-19, and FR-STATIC-18's `If-Range` sentence;
- FR-CFG-26, and FR-CFG-19's stage 8 note;
- FR-PROXY-32, all of it;
- the Non-goals change;
- Timeline item 8.

It touches FR-STATIC-11 (header list), FR-LOG-02/-05 (new kinds), NFR-RES-01 (worst case), NFR-RES-03 and DRV-07 (root list). The new text is `git diff 7c822d4..862e951 -- docs/`.

## Global Constraints

- **No C:** Rust only; the system libc and `libgcc_s` are the only C. The product's dependencies stay `libc` only (`scripts/check-product-deps.sh`).
- **`unsafe`:** only in `driver/src/sys/` and the two documented test sites (NFR-MAINT-05). This stage needs none.
- **Sandbox stays strict.** Ranges use the existing `sendfile(out, in, &off, len)` and `readahead(fd, offset, len)`. Both are allowed without argument filters on x86_64 and aarch64, glibc and musl (`driver/src/containment/filter.rs`). Backend files use the existing `openat2` beneath a root descriptor. So this stage adds **no new system call and no new argument**.

  If an implementer finds otherwise, stop and report. The allow-list then needs entries for every arch and libc, plus a binary test under `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`.
- **Code style:**
  - enums over bools;
  - exhaustive matches (no `_ =>` on enums);
  - files < 900 lines (split rather than grow `static_files.rs`, `proxy.rs`, `validate/mod.rs`, `forward/tests.rs`);
  - crate READMEs current in the same commit;
  - docs terse.
- **Strict red/green TDD.** The failing test and the code that passes it land as one commit, so the pushed branch never carries a failing test. Tests that pin existing behaviour are proven by a temporary mutation.
- **Spec current:**
  - Each task writes its "Planner calls" (listed in the task) into the spec under the requirement, as *Planner calls (stage 8 plan):*, in the same commit.
  - New log names go into FR-LOG-02/-05 with the code.
  - Statuses flip at stage end (Task 7).
- **Gate:** before every commit, run `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`. Also:
  - for driver changes, `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked`;
  - for sim net changes, `SIM_SEEDS=0..20000 cargo test -p durchlauferhitzer-sim --locked --test faults`.
- **New server-starting test files** go into CI's `SERVER_TESTS` (`.github/workflows/ci.yml`).
- **NFR-PERF ratchet:** static and proxy numbers may only get better (NFR-PERF-01/-02). Tasks 1 and 5 report `bench` medians (local, 5 runs) for their scenarios before and after. A regression past a target stops the task and is reported.

## Review Focus

1. **Range arithmetic at the edges** (Task 1), on a 1000-byte file unless noted:
   - `bytes=0-0`, `bytes=999-999` and `bytes=-1` each give one byte;
   - `bytes=0-18446744073709551615` clamps to `bytes 0-999/1000`;
   - `bytes=18446744073709551616-` overflows, is malformed, and gives `200`;
   - `bytes=1000-` and `bytes=-0` give `416`;
   - on an empty file every range gives `416 bytes */0`.
2. **A file that changes around a range** (Task 1):
   - `If-Range` is judged on the opened descriptor's stamp: a changed file is a whole `200` with the new `ETag`;
   - a file that shrinks below the range mid-transfer is cut (`FileAborted { Truncated }`), never a short `206`.
3. **Traversal through the backend header** (Tasks 4, 6). None of these may serve a byte outside the root; each is a `502` or a `404` per FR-PROXY-32:
   - `..`, `%2e%2e`, `%2F`, `%00`;
   - a symlink inside the root;
   - a sibling of the root (`/srv/files-old/a` for root `/srv/files`);
   - an absolute path elsewhere.
4. **File-header smuggling** (Tasks 3, 5):
   - Client side: forged headers in any case, on several lines, or named in `Connection` never reach a backend.
   - Backend side: both headers at once, the header twice, or the header on a `206`/`304`/`404` is never served off a non-`200` and never reaches a client.
5. **Drains, resets and pipelining around a backend file** (Task 5):
   - a shutdown drain finishes or aborts an opening or sending backend file exactly like a static one;
   - a client reset mid-file closes the file once (the upstream is already parked or closed);
   - a pipelined request behind a backend-file response is answered after it.

---

### Task 1: Range requests on static routes (FR-STATIC-19)

**Files:**
- Create: `core/src/http/range.rs` (+ `core/src/http/range/tests.rs`), `sim/tests/static_files/range.rs`, `sim/tests/faults/range.rs`, `driver/tests/file_ranges.rs`.
- Modify:
  - core: `core/src/http/mod.rs`, `core/src/http/request.rs` (`RequestHead.range`), `core/src/http/status.rs`, `core/src/http/response.rs`, `core/src/http/conditional.rs` (module doc: `If-Range` is FR-STATIC-19), `core/src/conn.rs` (`StaticRequest.range`; `SendingFile.size` renamed `end`), `core/src/core/static_files.rs`, `core/src/core/tests.rs` (`every_rendered_head_fits_the_send_buffer`);
  - sim: `sim/tests/support/model_http.rs` (file heads gain `Accept-Ranges`), `sim/tests/static_files/main.rs`, `sim/tests/faults/{generator,oracle,model}.rs`;
  - `.github/workflows/ci.yml` (`SERVER_TESTS` += `file_ranges`);
  - READMEs `core/README.md`, `sim/README.md`, `driver/README.md` (test list);
  - spec: FR-STATIC-11's header list gains `Accept-Ranges` (and `Content-Range` for a 206).

**Interfaces:**
- Produces, in `http::range`:
  - `pub struct RangeRequest { range: RangeSpec, if_range: IfRange }`, `Copy`, fixed size;
  - `RangeRequest::NONE`;
  - `pub fn of(headers: &[Header]) -> RangeRequest`;
  - `pub fn judge(&self, method: &Method, validators: &Validators, size: u64) -> Ranged`;
  - `pub enum Ranged { Whole, Part { first: u64, last: u64 }, Unsatisfiable }` (`last` inclusive).
  - Private: `enum RangeSpec { None, FromTo(u64, u64), From(u64), Suffix(u64), Ignored }`, where `Ignored` covers several ranges, malformed, another unit or several `Range` lines; `enum IfRange { Absent, Tag(FileStamp), Date(ImfFixdate), Never }`.
- Produces, in `http::status`: `Status::PartialContent` (206, `Partial Content`) and `Status::RangeNotSatisfiable` (416, `Range Not Satisfiable`).
- Produces, in `http::response`:
  - `render_file_head(content_type, part: Part, validators, persistence, date, out)` with `pub enum Part { Whole { size: u64 }, Range { first: u64, last: u64, size: u64 } }`;
  - `render_range_not_satisfiable(size: u64, persistence, body: BodyMode, date, out)`;
  - `pub const ACCEPT_RANGES: &str = "Accept-Ranges: bytes\r\n"`.
- Rules (FR-STATIC-19 plus planner calls):
  - The unit `bytes` is ASCII case-insensitive.
  - Empty list elements and OWS around elements are allowed (RFC 9110 §5.6.1).
  - `a-b` with `b < a` is malformed.
  - Digits only; a number over `u64` is malformed.
  - `b ≥ size` is clamped to `size - 1`.
  - `-n` with `n ≥ size` is the whole file as `0-(size-1)`.
  - `Range` counts only for `GET`.
  - `If-Range`, one line only:
    - a strong tag in this server's `ETag` form matches when its stamp equals the file's;
    - `W/…`, another tag or several lines never match;
    - a date matches only if its bytes equal the file's `Last-Modified` IMF-fixdate exactly.
  - `If-Range` without `Range` is ignored.
  - Decision order after `FileOpened` (regular file): `Conditional::not_modified` (304), then `RangeRequest::judge`.
- **Heads.**
  - `200` and `206`, in this order:
    1. status line;
    2. `Date`;
    3. `Content-Type`;
    4. `Content-Length`;
    5. `Content-Range: bytes a-b/size` (206 only);
    6. `ETag`;
    7. `Last-Modified`;
    8. `Accept-Ranges: bytes`;
    9. `X-Content-Type-Options: nosniff`;
    10. `Connection: close` (only when closing).
  - `416`:
    - It is `render_fixed`'s form with body `416 Range Not Satisfiable\n` (26 bytes).
    - It adds `Content-Range: bytes */size` and `Accept-Ranges: bytes` after `Content-Length`.
    - The file is closed at once, and persistence is kept.
  - `304`: unchanged (no `Accept-Ranges`).
- **Sending a range.**
  - `SendingFile { offset: first, end: last + 1, .. }`.
  - The first `Readahead` is `{ offset: first, len: min(end - first, FILE_CHUNK) }`, then chunk by chunk to `end`.
  - `on_file_change` and failures apply unchanged (`check_stamp` compares whole-file stamps).

- [ ] **Step 1: Failing tests.**
  - **`core/src/http/range/tests.rs`**, on a 1000-byte file. A table test `judges_ranges` asserts:

    | Input | `Ranged` |
    |---|---|
    | `bytes=0-99` | `Part{0,99}` |
    | `bytes=990-` | `Part{990,999}` |
    | `bytes=-10` | `Part{990,999}` |
    | `bytes=-2000` | `Part{0,999}` |
    | `bytes=500-5000` | `Part{500,999}` |
    | `BYTES=0-0` | `Part{0,0}` |
    | `bytes= 0-1 ,` | `Part{0,1}` |
    | `bytes=1000-` | `Unsatisfiable` |
    | `bytes=-0` | `Unsatisfiable` |
    | `bytes=0-1,5-6` | `Whole` |
    | `bytes=5-1` | `Whole` |
    | `items=0-1` | `Whole` |
    | `bytes=0x-1` | `Whole` |
    | `bytes=18446744073709551616-` | `Whole` |
    | two `Range` lines | `Whole` |
    | `HEAD` with `bytes=0-1` | `Whole` |
    | `Other("POST")` with `bytes=0-1` | `Whole` |

    Also in this file:
    - `empty_file_ranges_are_unsatisfiable`: `bytes=0-`, `bytes=-5` on size 0 give `Unsatisfiable`.
    - `if_range_keeps_or_drops_the_range`, all with `bytes=0-99`:
      - the file's own `ETag` gives `Part{0,99}`;
      - `W/` + it gives `Whole`;
      - `"x"` gives `Whole`;
      - exactly its `Last-Modified` gives `Part{0,99}`;
      - that date plus 1 s gives `Whole`;
      - the same second in RFC 850 form gives `Whole`;
      - two `If-Range` lines give `Whole`;
      - `If-Range` without `Range` gives `Whole`.
  - **`core/src/core/static_files/tests.rs`** (new tests; split the file if it passes 900 lines):
    - `range_gets_206_with_content_range`:
      - the exact head bytes for `bytes=0-99` on `a.txt` (1000 bytes, no date): `HTTP/1.1 206 Partial Content\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 100\r\nContent-Range: bytes 0-99/1000\r\nETag: …\r\nLast-Modified: …\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n`;
      - `Readahead { offset: 0, len: 100 }`;
      - `Log::Response { status: PartialContent }`.
    - `range_across_chunks_reads_from_its_first_byte`: a 600 KiB file with `bytes=200000-500000` gives `Readahead {200000, 262144}`, then `{462144, 37857}`, and `send_file` segments that sum to 300001.
    - `unsatisfiable_range_gets_416`: the exact head and body; `CloseFile` follows; keep-alive.
    - `whole_200_carries_accept_ranges`.
    - `not_modified_wins_over_range`: a matching `If-None-Match` plus `Range` gives `304`.
    - `changed_file_ignores_if_range`: an old `ETag` in `If-Range` gives `200` with the new `ETag`.
    - `range_on_head_is_a_whole_head`.
    - `range_file_shrinks_mid_transfer_is_cut`: `FileAborted { Truncated }` + `Close`.
    - `range_under_allow_growth_sends_exactly_the_range`.
    - `every_rendered_head_fits_the_send_buffer`: extended with a 206 and a 416 at `u64::MAX` sizes, `Date` and close.
  - **Sim:**
    - `sim/tests/static_files/range.rs`: the 206, 416 and `If-Range` cases end to end. Bytes are checked against `fs` content; a slow disk and a slow client are included.
    - `sim/tests/faults/range.rs`: random `Range` / `If-Range` lines (valid, suffix, open, several, malformed, other unit, `If-Range` matching / stale / weak / date). A model written from FR-STATIC-19 decides each answer and its bytes. The oracle checks every byte.
    - Coverage names, each required: `range-206`, `range-suffix`, `range-open-end`, `range-clamped`, `range-416`, `range-multi-200`, `range-malformed-200`, `if-range-206`, `if-range-200`, `range-304`, `range-head-200`, `range-across-chunk`.
    - Mutation proof (temporary, reported): off-by-one in `last` fails the oracle.
  - **Driver:** `driver/tests/file_ranges.rs`, through the binary (seccomp per the kill switch):
    - `range_crosses_a_chunk_boundary`: a 600 KiB pattern file with `bytes=200000-500000` gives exactly those bytes and `content-range: bytes 200000-500000/614400`;
    - `suffix_range_and_416`.
- [ ] **Step 2: Run, see them fail** (paste the RED output in the report).
- [ ] **Step 3: Implement.** The `RangeRequest` is read in `parse` next to `Conditional::of` and moved into `StaticRequest`. The decision lives in `range.rs`; `static_files.rs` only maps `Ranged` to states.
- [ ] **Step 4: Gate (both suites, `SIM_SEEDS=0..20000`), bench.** Report `static-small`, `static-large`, `conditional-hit` medians before and after; they must stay within NFR-PERF-01.
- [ ] **Step 5: Commit** "Static: single-range requests, If-Range, Accept-Ranges (FR-STATIC-19)", push.

Planner calls (into FR-STATIC-19):
- `Accept-Ranges` on 200, 206 and 416, not on 304.
- The header order above.
- The 416 body and its file closed at once.
- An empty file: every range is 416 (literal "does not overlap"; RFC 9110 §14.1.3 would serve a suffix range of an empty file).
- `If-Range` with a date means the exact `Last-Modified` bytes; RFC 9110 §13.1.5's "weak because modified within the `Date` second" check is not applied.
- Several `Range` lines are treated like several ranges (200).

### Task 2: Config `internal_files` (FR-CFG-26, FR-CFG-19 note), its root at startup and under Landlock

**Files:**
- Create: `core/src/config/validate/route.rs` (move `RouteKey`, `parse_route` and `Slot` out of `validate/mod.rs`, which is near 900), `core/src/config/validate/tests/internal_files.rs`.
- Modify:
  - core: `core/src/config/model.rs`, `core/src/config/mod.rs` (re-exports), `core/src/core/mod.rs` (`Core::new` adds each `internal_files` root to `static_roots`, deduplicated; doc of `static_roots`);
  - driver: `driver/tests/startup.rs`;
  - docs: `examples/durchlauferhitzer.conf`, `README.md` ("Try it" `mkdir` gains `/tmp/durchlauferhitzer/protected`; "every static root" and "beneath its static roots" become "static and `internal_files` roots"), `core/README.md`.

**Interfaces:**
- Produces, in `config::model`:
  - `ProxyRoute.internal_files: Option<InternalFiles>`;
  - `pub struct InternalFiles { pub header: FileHeader, pub root: PathBuf, pub on_file_change: OnFileChange }`;
  - `pub enum FileHeader { XSendfile, XAccelRedirect { prefix: String } }`;
  - `ConfigErrorKind::InternalFilesOnStatic`, displayed as "`internal_files` is only allowed on proxy routes";
  - `OnFileChangeOnProxy`'s message becomes "`on_file_change` is only allowed on static routes and proxy routes with `internal_files`";
  - `RouteKey::ALL` (like `LimitKey::ALL`).
- **Syntax:** `internal_files x-sendfile <root>;` or `internal_files x-accel-redirect <prefix> <root>;`. The errors:
  - wrong count for the form: `WrongValueCount("internal_files")`;
  - unknown form word: `InvalidValue { key: "internal_files", value: <word> }`;
  - bad root or prefix: `InvalidValue` with that value;
  - written twice: `DuplicateKey`.
- **Root:** `valid_root` (FR-CFG-15), and (planner call) only bytes 0x21–0x7E.
- **Prefix:**
  - it starts and ends with `/`;
  - `validate_path` accepts it (FR-CFG-13: FR-HTTP-12…16, no `?`);
  - `/` alone is fine.
- **`on_file_change`:** allowed on a proxy route with `internal_files` (default `abort`); on one without, still `OnFileChangeOnProxy`.
- **`Core::static_roots()`:** static roots, then `internal_files` roots, in config order, each path once. The driver's `open_roots`, `restrict_files` and the budget take it unchanged; the startup error keeps its wording `static root <path>: …`.

- [ ] **Step 1: Failing tests.**
  - **`validate/tests/internal_files.rs`:**
    - `x_sendfile_parses`: `internal_files x-sendfile /srv/files;` gives `Some(InternalFiles { header: XSendfile, root: "/srv/files", on_file_change: Abort })`.
    - `x_accel_redirect_parses`: `… x-accel-redirect /protected/ /srv/files;` gives `XAccelRedirect { prefix: "/protected/" }`; `/` as prefix is accepted.
    - `on_file_change_with_internal_files`: `allow_growth` gives `AllowGrowth`; without `internal_files` it is `OnFileChangeOnProxy` with the new message text.
    - `internal_files_on_static_is_an_error`: `InternalFilesOnStatic` at the setting's line:column.
    - `internal_files_twice_is_duplicate`.
    - `wrong_counts`: `x-sendfile;`, `x-sendfile /a /b;`, `x-accel-redirect /srv;`, `x-accel-redirect /p/ /a /b;` are all `WrongValueCount("internal_files")`.
    - `bad_values`: each is `InvalidValue` with the offending value:
      - the form word `nginx`;
      - roots `srv`, `/srv/`, `/a/../b`, `/my files`;
      - prefixes `/protected`, `protected/`, `/a?b/`, `/a//b/`, `/%61/`, `/./`.
  - **`core/src/core/tests.rs` `static_roots_include_internal_roots`:**
    - static `/srv/www` plus internal `/srv/files` gives `["/srv/www", "/srv/files"]`;
    - an internal root equal to a static one is listed once.
  - **`validate/tests/mod.rs` `example_config_loads`:** also asserts every `RouteKey::ALL` name appears as `"\n    <key> "`.
  - **`driver/tests/startup.rs` `missing_internal_files_root_is_exit_2`:** exit 2, and stderr names `static root <path>`.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.**
  - The example's `/api` route gains `internal_files x-accel-redirect /protected/ /tmp/durchlauferhitzer/protected;` and `on_file_change abort;`.
  - The comment explains in a few lines: the opt-in, which header, Rack's hints, 502 on a bad path, 404, that the backend's body is discarded, and ranges.
- [ ] **Step 4: Gate (both suites), commit** "Config: internal_files (FR-CFG-26)", push.

Planner calls (into FR-CFG-26):
- The root must be visible ASCII. No `X-Sendfile` value can match any other root, and the root goes into `X-Accel-Mapping`.
- `/` is a valid prefix.
- The startup message keeps "static root".

### Task 3: File headers stripped both ways, Rack hints set (FR-PROXY-32 Stripping, Hints)

**Files:**
- Create: `core/src/http/forward/file_header_tests.rs`, `sim/tests/proxy/internal_files.rs`.
- Modify:
  - core: `core/src/http/forward.rs`, `core/src/core/proxy.rs` (`Forward` and `ProxyRequest` carry the route's internal-files id), `core/src/conn.rs`, `core/src/core/relay.rs` (passes it to the renderer), `core/src/core/mod.rs` (`Core.internal_routes`);
  - sim: `sim/tests/proxy/main.rs`, `sim/tests/faults/proxy/{mod,request,response}.rs`;
  - READMEs.

**Interfaces:**
- Produces:
  - `pub(crate) struct InternalRouteId(u32)`, indexing `Core.internal_routes: Vec<InternalRoute>`;
  - `pub(crate) struct InternalRoute { files: InternalFiles, root: RootId }`, built once in `Core::new` (no per-request allocation);
  - `ProxyRequest.internal: Option<Arc<InternalRoute>>` *(as built in Task 3: `plan` does the one lookup; the handle carries the setting and its `RootId`)*.
- In `http::forward`:
  - `render_backend_request(head, path, peer, persistence, files: Option<&InternalFiles>, out)`;
  - `render_client_response(head, persistence, date, files: Option<&InternalFiles>, out)`;
  - each `*_len` twin gains the same parameter.
- **Toward the backend:**
  - `x-sendfile`, `x-accel-redirect`, `x-sendfile-type` and `x-accel-mapping` are dropped on every proxy route, like `CLIENT_IDENTITY`.
  - On an `internal_files` route, after `x-forwarded-proto`, the proxy appends `x-sendfile-type: X-Sendfile` or `x-sendfile-type: X-Accel-Redirect`.
  - On an `x-accel-redirect` route it also appends `x-accel-mapping: <root>/=<prefix>` (a root is never `/`: FR-CFG-26, controller ruling at Task 2).
- **Toward the client:** `x-sendfile` and `x-accel-redirect` are dropped on every route; on an `x-accel-redirect` route, every `x-accel-*` too.
- Until Task 5 every response is relayed. The trigger does not exist yet, so a `200` with the route's header is relayed without it. Task 5 changes exactly that case.

- [ ] **Step 1: Failing tests.**
  - **`forward/file_header_tests.rs`:**
    - `forged_file_headers_never_reach_the_backend`, on a plain, an `x-sendfile` and an `x-accel-redirect` route. The client head carries `X-Sendfile: /etc/passwd`, `x-accel-redirect: /x`, `X-SENDFILE-TYPE: X-Sendfile`, `X-Accel-Mapping: /=/x/`, plus `Connection: x-sendfile`. The rendered head holds none of the four names, except the hints, each exactly once.
    - `hints_on_an_x_sendfile_route`: exactly one `x-sendfile-type: X-Sendfile\r\n`, and no `x-accel-mapping`.
    - `hints_on_an_x_accel_redirect_route`: `x-sendfile-type: X-Accel-Redirect\r\nx-accel-mapping: /srv/files/=/protected/\r\n`.
    - `file_headers_never_reach_the_client`:
      - A backend `404` with `X-Sendfile: /a`, `X-Accel-Redirect: /b`, `X-Accel-Expires: 10`, `X-Accel-Buffering: no`: on plain and `x-sendfile` routes the first two go, and `x-accel-expires` and `x-accel-buffering` stay.
      - On an `x-accel-redirect` route all four go.
    - `len_twins_match_with_files`: the `*_len` equals the rendered length for every case above.
  - **`sim/tests/proxy/internal_files.rs`:**
    - `non_200_with_the_route_header_is_relayed_without_it`: a `404` with body `nope` and `X-Accel-Redirect` reaches the client with its body and without the header.
    - `the_other_header_on_a_200_is_dropped_and_ignored`: on an `x-sendfile` route, a `200` with `X-Accel-Redirect` is relayed as is, minus the header.
    - `backend_sees_the_hints`.
  - **Proxy net:**
    - Random configs give some routes `internal_files` (both forms).
    - Random client requests include the forged four (any case, several lines, listed in `Connection`), and random backend responses add file headers. To keep a trigger out until Task 5, the route's own header is only added on a non-`200`.
    - The request model asserts the backend never sees a forged one and sees the hints. The response model asserts no client ever sees a file header.
    - Coverage: `forged-file-header`, `file-header-on-non-200`, `other-file-header-on-200`, `x-accel-star-stripped`.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate (sim nets incl. `SIM_SEEDS=0..20000`), commit** "Proxy: strip file headers both ways, set Rack's sendfile hints (FR-PROXY-32)", push.

No planner call (the root `/` case is refused by FR-CFG-26).

### Task 4: Backend file paths (FR-PROXY-32 Path, Bad path)

A pure module; security-critical, reviewed on its own.

**Files:**
- Create: `core/src/http/backend_path.rs` (+ `core/src/http/backend_path/tests.rs`).
- Modify: `core/src/http/mod.rs`, `core/src/http/static_path.rs` (expose its `decode` as `pub(crate) fn decode_segment`), `core/README.md`.

**Interfaces:**
- Produces:
  - `pub fn sendfile_path(root: &[u8], value: &[u8]) -> Result<Located, BadPath>`;
  - `pub fn accel_path(prefix: &[u8], value: &[u8]) -> Result<Located, BadPath>`;
  - `pub enum Located { Open { segments: Vec<Vec<u8>>, slash: TrailingSlash }, NotFound }`;
  - `pub enum BadPath { Empty, BadByte, Query, NoMatch, BadSegment, BadEscape }`.
- **Checks, in order:**
  1. empty gives `Empty`;
  2. any byte outside 0x21–0x7E gives `BadByte`;
  3. `?` gives `Query`;
  4. the start must match, else `NoMatch`:
     - `x-sendfile`: the root followed by `/`, or exactly the root (a root is never `/`, FR-CFG-26);
     - `x-accel-redirect`: the prefix, by raw bytes, case-sensitive;
  5. the rest's raw segments: an empty segment other than the trailing one, `.` or `..` gives `BadSegment`;
  6. `x-accel-redirect` only: each segment is percent-decoded once; a malformed escape gives `BadEscape`;
  7. then the static rules: a segment starting with `.`, or a decoded `/` or NUL, gives `Located::NotFound` (FR-STATIC-09).
- An empty rest gives `Open { segments: [], slash: Absent }`, which opens the root: a directory, so a 404 later. A trailing `/` gives `slash: Present`.
- `x-sendfile` segments are taken as bytes, not decoded.

- [ ] **Step 1: Failing tests** (table tests with exact values).
  - **`sendfile_path(b"/srv/files", …)`:**

    | Value | Result |
    |---|---|
    | `/srv/files/a/b.pdf` | `Open{[a, b.pdf], Absent}` |
    | `/srv/files/a%20b` | `Open{[a%20b]}` |
    | `/srv/files/` | `Open{[], Present}` |
    | `/srv/files` | `Open{[], Absent}` |
    | `/srv/files/a/` | `Open{[a], Present}` |
    | `/srv/filesx/a` | `NoMatch` |
    | `/srv/files-old/a` | `NoMatch` |
    | `/etc/passwd` | `NoMatch` |
    | `` (empty) | `Empty` |
    | `/srv/files/a b` | `BadByte` |
    | `/srv/files/\x7f` | `BadByte` |
    | `/srv/files/\xc3\xa4` | `BadByte` |
    | `/srv/files/a?x` | `Query` |
    | `/srv/files//a` | `BadSegment` |
    | `/srv/files/./a` | `BadSegment` |
    | `/srv/files/a/../b` | `BadSegment` |
    | `/srv/files/a//` | `BadSegment` |
    | `/srv/files/.env` | `NotFound` |
    | `/srv/files/a/.git/x` | `NotFound` |

  - **`accel_path(b"/protected/", …)`:**

    | Value | Result |
    |---|---|
    | `/protected/a/b.pdf` | `Open{[a, b.pdf]}` |
    | `/protected/a%20b` | `Open{[a b]}` |
    | `/protected/%C3%A4` | `Open{[[0xC3, 0xA4]]}` |
    | `/protected/%252e` | `Open{[%2e]}` |
    | `/protected/` | `Open{[], Absent}` |
    | `/protected/a/` | `Open{[a], Present}` |
    | `/protected` | `NoMatch` |
    | `/Protected/a` | `NoMatch` |
    | `/other/a` | `NoMatch` |
    | `/protected/%zz` | `BadEscape` |
    | `/protected/%2` | `BadEscape` |
    | `/protected/%2e%2e/x` | `NotFound` |
    | `/protected/a%2Fb` | `NotFound` |
    | `/protected/%00` | `NotFound` |
    | `/protected/../x` | `BadSegment` |
    | `/protected//x` | `BadSegment` |
    | `/protected/a?b` | `Query` |
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate, commit** "Proxy: backend file path rules (FR-PROXY-32)", push.

Planner calls (into FR-PROXY-32):
- A malformed `%` escape in an `X-Accel-Redirect` value is a bad path (502): the spec's 404 list is what a static route answers, and a static route never sees one.
- For `x-sendfile`, the root without its `/` counts as "the root alone" (404).
- A decoded `.`-segment (`%2e…`) is a dotfile (404), while a raw `.` or `..` segment is a bad path (502).

### Task 5: Serving the backend's file (FR-PROXY-32 Trigger, 404, client's response, backend body)

**Files:**
- Create:
  - core: `core/src/core/backend_file.rs` (the trigger and the post-open answer), `core/src/core/file_answer.rs` (shared post-open decision for both sources, moved out of `static_files.rs`), `core/src/core/backend_file/tests.rs`.
- Modify:
  - core: `core/src/core/relay.rs` (branch to the trigger before the HTTP/1.0 `Transfer-Encoding` and head-size checks), `core/src/core/static_files.rs`, `core/src/conn.rs`, `core/src/core/mod.rs` (`send_buffer`, `send_capacity`, `send_file`; `sent` in `request.rs`), `core/src/http/forward.rs` (kept-lines renderer), `core/src/http/response.rs`, `core/src/log.rs`, `core/src/config/model.rs` (`worst_case_per_connection`), `core/src/config/validate/mod.rs` (`check_memory_budget`), `core/src/core/contract.rs` (docs: `OpenFile` from a proxy exchange; `send_buffer` while a backend file's head is out), `core/src/upstream.rs` (head buffer capacity on `internal_files` routes);
  - driver: `driver/src/log/format.rs` (+ its tests);
  - sim: `sim/tests/proxy/internal_files.rs`;
  - spec: FR-LOG-02, FR-LOG-05 (kind and failure names), NFR-RES-01 (the extra head bytes);
  - READMEs `core/README.md`, `driver/README.md`, `sim/README.md`.

**Interfaces:**
- **In `log`:**
  - `Log::BackendFile { conn, peer, backend: BackendId, status: Status, request: RequestSummary }`, logged once for a `200`, `206` or `304`;
  - `BackendFailure::BadFilePath`.
  - Driver format: `kind=backend_file conn peer backend status method [host] path`, and failure `bad_file_path`.
- **In `config::model`:**
  - extend Task 3's `pub enum RouteMix { StaticOnly, Proxy { hint_bytes } }` (from `Config::route_mix()`) with the file-head term, e.g. `Proxy { hint_bytes, file_head: usize }` (`FILE_HEAD_EXTRA` when any route has `internal_files`, else 0);
  - `worst_case_per_connection(limits: &Limits, mix: RouteMix) -> u64` adds it.
- **In `http::response`:**
  - `pub const FILE_HEAD_EXTRA: usize`: the most bytes the core adds around a backend's kept lines — status line, `Date`, `Content-Type` (longest MIME), `Content-Length`, `Content-Range` (`u64::MAX` values), `ETag`, `Last-Modified`, `Accept-Ranges`, `X-Content-Type-Options`, `Connection: close`;
  - `render_backend_file_head(status: FileStatus, kept: &[u8], has: Kept, content_type: &str, part: Part, validators, persistence, date, out)`, with `pub enum FileStatus { Ok, Partial, NotModified }`.
- **In `http::forward`:**
  - `render_kept_lines(head: &ResponseHead, files: &InternalFiles, out: &mut Vec<u8>) -> Kept`, plus its `kept_lines_len` twin;
  - `pub struct Kept { pub content_type: Sent, pub nosniff: Sent, pub date: Sent }`, with `pub enum Sent { Yes, No }`.
  - It writes the backend's headers lowercase, after FR-PROXY-12 and the Task 3 stripping. It drops `content-length`, `transfer-encoding`, `etag`, `last-modified`, `accept-ranges` and `content-range`.
- **In `conn`:**
  - `StaticRequest.source: FileSource`, with `pub(crate) enum FileSource { Route, Backend(BackendFile) }` and `pub(crate) struct BackendFile { backend: BackendId, head: Vec<u8>, kept: Kept }`;
  - `OpenStep::Backend`: a directory is a 404 and opens no index;
  - `ConnState::SendingFile.head: FileHead`, with `pub(crate) enum FileHead { SendBuffer, Own { bytes: Vec<u8>, sent: usize } }`.
  - `send_buffer` gives `c.send`, then the own head's unsent bytes. `send_file` waits for both to be empty. `send_capacity` reports the own head's capacity while it is pending.
  - A backend file's `304` and `HEAD` are `SendingFile { chunk: Chunk::Done, .. }` with the own head; the file closes when the head is out.
- **The trigger,** in `relay`, when the request's route has `internal_files` and the head's status is 200:
  - The route's header absent: relay as Task 3.
  - Present on two or more lines, or `BadPath`: `fail_before_head(BackendFailure::BadFilePath)`, which logs `BackendFailed`, then `Response` 502, then closes the upstream.
  - `Located::NotFound`: the plain 404 below.
  - `Open`:
    1. If `kept_lines_len + FILE_HEAD_EXTRA > head capacity`, `fail_before_head(BadResponse(HeadTooLarge))` (NFR-RES-03; planner call: judged before the open with the bound).
    2. Else render the kept lines into `buffers.head`.
    3. Release the upstream with `Release::Park` only if the head says `Content-Length: 0` and today's reuse rules hold (FR-PROXY-02); else `Release::Close`. The backend body is never read, and bytes after the head close it.
    4. Drop the receive buffer.
    5. `OpenFile { root: internal root, path: segments }`.
  - No `max_response_body`, and no `TransferEncodingToHttp10` here.
  - The client's persistence: the request's, or `Close` when its body was not fully read (FR-PROXY-27/-28).
  - The client's `Conditional` and `RangeRequest` travel in the `StaticRequest`, which is built from `ProxyRequest`. `ProxyRequest` keeps both (fixed size) on `internal_files` routes.
- **After the open,** with the shared decision of Task 1:
  - `200`, `206` or `304`: `render_backend_file_head`.
    - The status line is the core's.
    - Then `Date` (the core's, only if the backend sent none) and the kept lines in order.
    - Then the core lines in Task 1's order. `Content-Type` is added only if the backend sent none (else FR-STATIC-12 by the last segment). `X-Content-Type-Options: nosniff` is added only if the backend sent none.
    - A `304` adds only `ETag` and `Last-Modified` to the kept lines.
  - `416`, `404` (`FileFailed` of an open, `Directory`, `Other`, a regular file with `slash: Present`, `Located::NotFound`): the plain static responses, backend headers dropped, logged as `Response`.
  - `HEAD`: FR-STATIC-13.
  - `Range` counts only for `GET` (a `POST` answered with a file gets the whole 200).
  - `on_file_change` is the route's `internal_files` setting.

- [ ] **Step 1: Failing tests** (`core/src/core/backend_file/tests.rs`, `x-accel-redirect /protected/` → root `/srv/files` unless noted; file `a.pdf`, 1000 bytes):
  - **Opening and the upstream:**
    - `route_header_on_200_opens_the_file`: `OpenFile { root: <internal RootId>, path: [b"a.pdf"] }`. No upstream read is offered after the head, and the backend body `hello` never reaches the client.
    - `upstream_parked_only_after_content_length_0`:
      - `Content-Length: 0` on a pooled HTTP/1.1 exchange: no `CloseUpstream`; the upstream is held by the pool;
      - `Content-Length: 5`, chunked, close-delimited, `Connection: close`, a HEAD request, or bytes behind the head: `CloseUpstream`.
  - **The merged head:**
    - `merged_head_keeps_backend_headers`:
      - The backend sends `200 OK` with `Content-Type: application/x-custom`, `Content-Disposition: attachment; filename="a.pdf"`, `Cache-Control: private`, `Content-Length: 0`, `ETag: "b"`, `Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT`, `Accept-Ranges: none` and `X-Accel-Redirect: /protected/a.pdf`.
      - The client head is exactly `HTTP/1.1 200 OK\r\ncontent-type: application/x-custom\r\ncontent-disposition: attachment; filename="a.pdf"\r\ncache-control: private\r\nContent-Length: 1000\r\nETag: <file's>\r\nLast-Modified: <file's>\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n`.
    - `content_type_from_the_name_without_a_backend_one`: `Content-Type: application/pdf` in the core's place.
    - `backend_date_kept_core_date_otherwise`.
  - **Conditionals, ranges, HEAD, methods:**
    - `conditional_304_keeps_backend_headers`: `HTTP/1.1 304 Not Modified\r\ncontent-type: application/x-custom\r\n…cache-control: private\r\nETag: …\r\nLast-Modified: …\r\n\r\n`; `CloseFile`; `Log::BackendFile { status: NotModified }`.
    - `range_206_keeps_backend_headers`: `bytes=0-99` gives `Content-Range: bytes 0-99/1000` and the 100 bytes.
    - `range_416_is_plain`: Task 1's 416 exactly; logged as `Response`.
    - `head_request_gets_head_only`.
    - `post_gets_the_file_and_range_is_ignored`: a POST with `Content-Length: 3` written whole and `Range: bytes=0-1` gives a `200` of 1000 bytes; keep-alive.
  - **Not found and bad paths:**
    - `missing_file_is_a_plain_404`: `FileFailed { NotFound }` gives `render_fixed(NotFound)` exactly. The log is `[Response 404]` only: no `BackendFile`, no `BackendFailed`.
    - `directory_and_trailing_slash_are_404`.
    - `dotfile_is_404`.
    - `bad_paths_are_502`: for one case of each `BadPath`, and for the header on two lines: `Log::BackendFailed { kind: BadFilePath }`, then `Log::Response { status: BadGateway }`, then `CloseUpstream`. There is no `OpenFile`.
  - **No proxy limits on the file:**
    - `chunked_head_to_http10_client_is_served`: an HTTP/1.0 client, and a backend `Transfer-Encoding: chunked` head with the header, give a 200 file with no `transfer-encoding`.
    - `max_response_body_does_not_apply`: limit 10, file 1000, backend `Content-Length: 5000` gives a 200.
  - **Logging and file changes:**
    - `served_file_logged_once`: logs `[BackendFile { status: Ok, backend, request }]` only; no `ProxiedResponse`.
    - `on_file_change_from_internal_files`: `abort` gives `FileAborted { Changed }` + `Close` on a change mid-transfer; `allow_growth` sends exactly 1000 bytes.
  - **Bounds:**
    - `merged_head_bound_is_502`: the pure check `fits_backend_file(kept_len, capacity)` at the boundary: `capacity - FILE_HEAD_EXTRA` fits, one more does not. (The branch is unreachable by events once the capacity grows; said in its doc comment.)
    - `file_head_extra_bounds_the_core_lines`: the worst-case 206 core lines (`u64::MAX` range and size, `text/javascript; charset=utf-8`, a `Date`, close) take at most `FILE_HEAD_EXTRA` bytes.
    - `worst_case_per_connection_for_defaults` (extended): with `internal_files`, the Task 3 worst case plus `FILE_HEAD_EXTRA`; the memory budget uses `Config::route_mix()`.
  - **Review Focus 5:**
    - `shutdown_drain_finishes_a_sending_backend_file`;
    - `abort_all_cuts_an_opening_backend_file`;
    - `client_reset_mid_backend_file_closes_the_file_once`;
    - `pipelined_request_behind_a_backend_file_is_answered_after_it`.
  - **Driver** `driver/src/log/format.rs` tests: `kind=backend_file conn=3 peer=192.0.2.7 backend=127.0.0.1:9000 status=206 method=GET host=example.com path=/dl/a` and `failure=bad_file_path`. The sim's contract checks run in `sim/tests/proxy/internal_files.rs` for each core case above end to end. A slow disk and a slow client are included.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** `static_files.rs` and `backend_file.rs` share `file_answer.rs`, so the 304 / range / head decision exists once.
- [ ] **Step 4: Gate (both suites, sim nets), bench `proxy-small`** (must stay within NFR-PERF-02). Commit "Proxy: serve backend-directed files (FR-PROXY-32)", push.

Planner calls (into FR-PROXY-32 and FR-LOG-02/-05):
- **Logging:**
  - log kind `BackendFile` / `backend_file` for `200`, `206` and `304`;
  - `416` and `404` are `Response` with backend headers dropped (the `416` like the `404`);
  - failure `BadFilePath` / `bad_file_path`, with no sub-reason.
- **The merged head:**
  - the core's status line, not the backend's reason;
  - the backend's `Accept-Ranges` and `Content-Range` are replaced too;
  - the backend's `X-Content-Type-Options` wins like its `Content-Type`;
  - header order as above (the backend's lines lowercase, the core's capitalised).
- **Bounds and request state:**
  - the 502 for an oversized merged head is judged before the open, against `FILE_HEAD_EXTRA`;
  - the route's settings travel by the `Arc<InternalRoute>` handle `plan` resolved (Task 3), not by copy or a second lookup.

### Task 6: Through the real binary, the random proxy net, and a bench scenario

**Files:**
- Create: `driver/tests/internal_files.rs`, `sim/tests/faults/proxy/internal.rs`.
- Modify:
  - `.github/workflows/ci.yml` (`SERVER_TESTS` += `internal_files`);
  - sim: `sim/tests/faults/proxy/{mod,response,outcome,check,draw}.rs` (keep each under 900 lines; the new model lives in `internal.rs`);
  - bench: `bench/src/{scenario,backend,main}.rs`, `bench/README.md`;
  - READMEs `driver/README.md`, `sim/README.md`.

**Interfaces:**
- Consumes everything above. Produces no product code (fix rounds may).
- **Bench:** scenario `proxy-file`. A keep-alive stub backend answers every request `200`, `Content-Length: 0`, `X-Accel-Redirect: /protected/1k.bin`, and the server sends the 1 KiB file. It is in `Scenario::DEFAULT`, and the nightly records it. It gets no NFR-PERF target until the owner sets one from its first nightly (planner call). Reason: this path exists to save backend work, and a nightly row is how a regression would show.

- [ ] **Step 1: Failing tests.**
  - **`driver/tests/internal_files.rs`:** the binary with a std stub backend. Containment `landlock required` when `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1`, else `best_effort`; seccomp per the kill switch.
    - `x_accel_redirect_serves_the_file_under_containment`: the 600 KiB pattern file arrives exactly. `content-disposition` is kept, and the log line is `kind=backend_file … status=200`.
    - `x_sendfile_range_crosses_a_chunk`: `bytes=200000-500000` gives exactly those bytes.
    - `outside_the_root_is_502`: `X-Sendfile: /etc/passwd` gives a 502 and `failure=bad_file_path`, and no byte of `/etc/passwd` is sent.
    - `symlink_in_the_root_is_404`: a symlink inside the root pointing outside gives a 404.
    - `client_cannot_forge_file_headers`: the backend's recorded request has no forged header and has the hints.
    - `pooled_after_content_length_0`: two sequential requests use one backend connection.
  - **`sim/tests/faults/proxy/internal.rs`:**
    - **Net:** random `internal_files` routes, both forms. Each gets a random file tree under its root in the sim fs: regular files of 0–600 KiB, directories, dotfiles, symlinks, denied and missing entries, and files that change while being sent.
    - **Backends:** answers carry the route's header with a valid path, each `BadPath` class, a traversal attempt, two header lines, the other header, or the header on a non-`200`; and any body framing.
    - **Clients:** random methods, plus random `Range` / `If-Range` / `If-None-Match` lines.
    - **Model,** from FR-PROXY-32 and FR-STATIC-19: the status, the merged head, the exact file bytes, the upstream's fate (parked only after `Content-Length: 0`), and the log kinds.
    - **Coverage, each required:** `file-200`, `file-206`, `file-304`, `file-416`, `file-404-missing`, `file-404-dir`, `file-404-dotfile`, `file-502-bad-path`, `file-502-repeated`, `file-head`, `file-post`, `file-parked`, `file-closed-body`, `file-changed-abort`, `file-backend-content-type`, `file-name-content-type`.
    - **Mutation proof** (temporary, reported): each of these must fail the oracle — serving a `BadSegment` path, relaying the backend body after the head, and parking after `Content-Length: 5`.
  - **Bench:** a `bench` unit test that the `proxy-file` stub answers with the header and `Content-Length: 0`.
- [ ] **Step 2: Run, see them fail** where code is missing (the bench scenario, the net's model). The driver tests pin behaviour built in Tasks 1–5: prove each by a temporary core mutation (skip the bad-path check, relay the backend body) and paste the failing run.
- [ ] **Step 3: Implement** the test code and the scenario. Run `bench` 5× and report the `proxy-file` medians next to `proxy-small` and `static-small`.
- [ ] **Step 4: Gate (both suites, `SIM_SEEDS=0..20000`, the kill-switch run, `cargo check --manifest-path bench/Cargo.toml --locked`), commit** "Tests: backend-directed files through the binary and the random proxy net; bench proxy-file", push.

### Task 7: Stage close-out (controller, inline)

- [ ] **Spec statuses** `[done]` for:
  - FR-STATIC-19, FR-CFG-26 and FR-PROXY-32;
  - FR-CFG-19's stage 8 note;
  - Timeline item 8;
  - FR-STATIC-18's "(stage 8; ignored before)", reworded to current.
- [ ] **Doc wording:** DRV-07 and the contract say `static_roots()` lists static and `internal_files` roots. The README's containment paragraph is current. FR-CFG-24 names stage 8's key.
- [ ] **HANDOFF** updated, with carry-forwards:
  - the `proxy-file` target, for the owner;
  - multipart ranges, still not served;
  - nginx's other `X-Accel-*` features, unsupported.
- [ ] **Then** the stage 8 whole-branch review.
