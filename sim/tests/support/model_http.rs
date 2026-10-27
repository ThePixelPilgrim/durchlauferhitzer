//! The model of the responses the core writes itself, written from the
//! spec (FR-HTTP-10, -19…-22a, -06, FR-STATIC-06, -11…-13) rather than taken
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

use durchlauferhitzer_core::http::request::{HttpError, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;

/// The model's reason phrase for each status the core sends.
pub(crate) fn model_reason(status: Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::MovedPermanently => "Moved Permanently",
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

/// The head of a `200 OK` file response (FR-STATIC-11): status line,
/// `Date` (FR-HTTP-22a, once set), `Content-Type` (FR-STATIC-12, from the model's own table in
/// `model.rs`), `Content-Length` = the size at open,
/// `X-Content-Type-Options: nosniff`, `Connection: close` only when
/// closing, blank line. A `HEAD` gets the same head (FR-STATIC-13).
pub(crate) fn model_file_head(
    content_type: &str,
    size: u64,
    persistence: Persistence,
    date: Option<&str>,
) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\n{}Content-Type: {content_type}\r\nContent-Length: {size}\r\n\
         X-Content-Type-Options: nosniff\r\n",
        date_line(date)
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
