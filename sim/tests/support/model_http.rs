//! The model of the responses the core writes itself, written from the
//! spec (FR-HTTP-10, -19…-22a, -06, FR-STATIC-06, -11…-13, -18) rather than taken
//! from the core's renderers, so a wrong status, reason, header or body in
//! the core shows up in the tests. Shared by every test binary of the sim
//! (`#[path]` include), which take their expected bytes from here.
//!
//! `date` is the IMF-fixdate the test set with `Sim::set_date`, written
//! out by the test (never computed by the core's code); `None` while no
//! date was set, and then no response carries `Date` (FR-HTTP-22a).

/// The example IMF-fixdate of RFC 9110 section 5.6.7, as Unix seconds
/// and as text: the date tests that check `Date` set.
#[allow(dead_code, reason = "not every test binary checks Date")]
pub(crate) const RFC_DATE: (u64, &str) = (784_111_777, "Sun, 06 Nov 1994 08:49:37 GMT");

/// `Date: <date>\r\n` (FR-HTTP-22a), right after the status line of every
/// response the core renders itself, once a date is set.
fn date_line(date: Option<&str>) -> String {
    date.map(|d| format!("Date: {d}\r\n")).unwrap_or_default()
}

use durchlauferhitzer_core::FileStamp;
use durchlauferhitzer_core::http::request::{HttpError, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;

/// The model's reason phrase for each status the core sends.
pub(crate) fn model_reason(status: Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::MovedPermanently => "Moved Permanently",
        Status::NotModified => "Not Modified",
        Status::BadRequest => "Bad Request",
        Status::NotFound => "Not Found",
        Status::MethodNotAllowed => "Method Not Allowed",
        Status::RequestTimeout => "Request Timeout",
        Status::ContentTooLarge => "Content Too Large",
        Status::UriTooLong => "URI Too Long",
        Status::ExpectationFailed => "Expectation Failed",
        Status::RequestHeaderFieldsTooLarge => "Request Header Fields Too Large",
        Status::BadGateway => "Bad Gateway",
        Status::ServiceUnavailable => "Service Unavailable",
        Status::GatewayTimeout => "Gateway Timeout",
        Status::HttpVersionNotSupported => "HTTP Version Not Supported",
    }
}

/// The model's numeric code for each status.
pub(crate) fn model_code(status: Status) -> u16 {
    match status {
        Status::Ok => 200,
        Status::MovedPermanently => 301,
        Status::NotModified => 304,
        Status::BadRequest => 400,
        Status::NotFound => 404,
        Status::MethodNotAllowed => 405,
        Status::RequestTimeout => 408,
        Status::ContentTooLarge => 413,
        Status::UriTooLong => 414,
        Status::ExpectationFailed => 417,
        Status::RequestHeaderFieldsTooLarge => 431,
        Status::BadGateway => 502,
        Status::ServiceUnavailable => 503,
        Status::GatewayTimeout => 504,
        Status::HttpVersionNotSupported => 505,
    }
}

/// The status a parse error is answered with (FR-HTTP-01, -06, -10,
/// FR-PROXY-25): 414 for a long request line, 431 for a head too large or
/// with too many headers, 505 for an unsupported version, 417 for a bad
/// `Expect`, 400 for everything else.
pub(crate) fn model_http_status(e: &HttpError) -> Status {
    match e {
        HttpError::RequestLineTooLong => Status::UriTooLong,
        HttpError::HeadTooLarge | HttpError::TooManyHeaders => Status::RequestHeaderFieldsTooLarge,
        HttpError::UnsupportedVersion => Status::HttpVersionNotSupported,
        HttpError::BadExpect => Status::ExpectationFailed,
        HttpError::BadRequestLine
        | HttpError::BadMethod
        | HttpError::BadVersion
        | HttpError::BadTarget(_)
        | HttpError::BareLf
        | HttpError::BadHeaderName
        | HttpError::BadHeaderValue
        | HttpError::ObsFold
        | HttpError::MissingHost
        | HttpError::DuplicateHost
        | HttpError::BadHost
        | HttpError::ContentLengthWithTransferEncoding
        | HttpError::BadContentLength
        | HttpError::DuplicateContentLength
        | HttpError::UnsupportedTransferEncoding
        | HttpError::TransferEncodingInHttp10
        | HttpError::ConnectionNamesFraming => Status::BadRequest,
    }
}

/// A fixed response as FR-HTTP-19 describes it: status line, `Date`
/// (FR-HTTP-22a, once set), `Content-Type`, `Content-Length`, `X-Content-Type-Options`, `Allow` for
/// a 405 (FR-HTTP-20), `Connection: close` only when the connection will
/// close, blank line, body `"<code> <reason>\n"` (left out for HEAD,
/// FR-HTTP-21, with the full `Content-Length`).
pub(crate) fn model_fixed(
    status: Status,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&str>,
) -> Vec<u8> {
    let (code, reason) = (model_code(status), model_reason(status));
    let text = format!("{code} {reason}\n");
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\n{}Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nX-Content-Type-Options: nosniff\r\n",
        date_line(date),
        text.len()
    );
    if status == Status::MethodNotAllowed {
        out.push_str("Allow: GET, HEAD\r\n");
    }
    if persistence == Persistence::Close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    if body == BodyMode::Full {
        out.push_str(&text);
    }
    out.into_bytes()
}

/// The IMF-fixdate of `secs` seconds after 1970-01-01 (RFC 9110 section
/// 5.6.7), counted out year by year and month by month: a different way
/// than the core's day arithmetic.
pub(crate) fn model_imf_fixdate(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut days = secs / 86_400;
    let weekday = DAYS.get((days % 7) as usize).copied().unwrap_or_default();
    let mut year = 1970;
    loop {
        let len = if model_leap(year) { 366 } else { 365 };
        if days < len {
            break;
        }
        days -= len;
        year += 1;
    }
    let mut month = 0;
    while days >= model_month_len(year, month) {
        days -= model_month_len(year, month);
        month += 1;
    }
    let of_day = secs % 86_400;
    format!(
        "{weekday}, {:02} {} {year} {:02}:{:02}:{:02} GMT",
        days + 1,
        MONTHS.get(month).copied().unwrap_or_default(),
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

pub(crate) fn model_leap(year: u64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days of `month` (0 = January) in `year`.
pub(crate) fn model_month_len(year: u64, month: usize) -> u64 {
    match month {
        1 if model_leap(year) => 29,
        1 => 28,
        3 | 5 | 8 | 10 => 30,
        _ => 31,
    }
}

/// The stamp of a file of `size` bytes that was the `nth` file added to
/// its `Sim` (`Sim::add_file` gives the files mtimes of 1, 2, ... ns).
pub(crate) fn sim_stamp(size: u64, nth: u64) -> FileStamp {
    FileStamp {
        size,
        modified_ns: nth,
    }
}

/// A file's `ETag` (FR-STATIC-18): strong, size and mtime in nanoseconds
/// as lowercase hex, `"<size>-<mtime>"`.
pub(crate) fn model_etag(stamp: FileStamp) -> String {
    format!("\"{:x}-{:x}\"", stamp.size, stamp.modified_ns)
}

/// `ETag` and `Last-Modified` (the mtime in whole seconds) lines.
fn validator_lines(stamp: FileStamp) -> String {
    format!(
        "ETag: {}\r\nLast-Modified: {}\r\n",
        model_etag(stamp),
        model_imf_fixdate(stamp.modified_ns / 1_000_000_000)
    )
}

/// The head of a `200 OK` file response (FR-STATIC-11): status line,
/// `Date` (FR-HTTP-22a, once set), `Content-Type` (FR-STATIC-12, from the model's own table in
/// `model.rs`), `Content-Length` = the size at open, `ETag` and
/// `Last-Modified` of the stamp at open (FR-STATIC-18),
/// `X-Content-Type-Options: nosniff`, `Connection: close` only when
/// closing, blank line. A `HEAD` gets the same head (FR-STATIC-13).
pub(crate) fn model_file_head(
    content_type: &str,
    stamp: FileStamp,
    persistence: Persistence,
    date: Option<&str>,
) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\n{}Content-Type: {content_type}\r\nContent-Length: {}\r\n\
         {}X-Content-Type-Options: nosniff\r\n",
        date_line(date),
        stamp.size,
        validator_lines(stamp)
    );
    if persistence == Persistence::Close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// A `304 Not Modified` (FR-STATIC-18): status line, `Date` (once set),
/// `ETag`, `Last-Modified`, `Connection: close` only when closing; no
/// other header and no body, for `GET` and `HEAD` alike.
pub(crate) fn model_not_modified(
    stamp: FileStamp,
    persistence: Persistence,
    date: Option<&str>,
) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 304 Not Modified\r\n{}{}",
        date_line(date),
        validator_lines(stamp)
    );
    if persistence == Persistence::Close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// A directory redirect (FR-STATIC-06): `301 Moved Permanently` with
/// `Location`, then the headers of every fixed response (FR-HTTP-19) for
/// the fixed body `"301 Moved Permanently\n"`, which a `HEAD` leaves out
/// (FR-HTTP-21). The spec does not order `Location` among the headers;
/// the model puts it right after the status line and its `Date`.
pub(crate) fn model_redirect(
    location: &str,
    persistence: Persistence,
    body: BodyMode,
    date: Option<&str>,
) -> Vec<u8> {
    const BODY: &str = "301 Moved Permanently\n";
    let mut out = format!(
        "HTTP/1.1 301 Moved Permanently\r\n{}Location: {location}\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\n",
        date_line(date),
        BODY.len()
    );
    if persistence == Persistence::Close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    if body == BodyMode::Full {
        out.push_str(BODY);
    }
    out.into_bytes()
}
