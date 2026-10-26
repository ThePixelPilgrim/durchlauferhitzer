//! Status codes the server can emit, and the mapping from parse errors.

use super::forward::ForwardError;
use super::request::HttpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    MovedPermanently,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    RequestTimeout,
    ContentTooLarge,
    UriTooLong,
    ExpectationFailed,
    RequestHeaderFieldsTooLarge,
    NotImplemented,
    BadGateway,
    ServiceUnavailable,
    GatewayTimeout,
    HttpVersionNotSupported,
}

impl Status {
    pub fn code(self) -> u16 {
        match self {
            Status::Ok => 200,
            Status::BadRequest => 400,
            Status::MovedPermanently => 301,
            Status::NotFound => 404,
            Status::MethodNotAllowed => 405,
            Status::RequestTimeout => 408,
            Status::ContentTooLarge => 413,
            Status::UriTooLong => 414,
            Status::ExpectationFailed => 417,
            Status::RequestHeaderFieldsTooLarge => 431,
            Status::NotImplemented => 501,
            Status::BadGateway => 502,
            Status::ServiceUnavailable => 503,
            Status::GatewayTimeout => 504,
            Status::HttpVersionNotSupported => 505,
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::BadRequest => "Bad Request",
            Status::MovedPermanently => "Moved Permanently",
            Status::NotFound => "Not Found",
            Status::MethodNotAllowed => "Method Not Allowed",
            Status::RequestTimeout => "Request Timeout",
            Status::ContentTooLarge => "Content Too Large",
            Status::UriTooLong => "URI Too Long",
            Status::ExpectationFailed => "Expectation Failed",
            Status::RequestHeaderFieldsTooLarge => "Request Header Fields Too Large",
            Status::NotImplemented => "Not Implemented",
            Status::BadGateway => "Bad Gateway",
            Status::ServiceUnavailable => "Service Unavailable",
            Status::GatewayTimeout => "Gateway Timeout",
            Status::HttpVersionNotSupported => "HTTP Version Not Supported",
        }
    }
}

impl HttpError {
    pub fn status(&self) -> Status {
        match self {
            HttpError::RequestLineTooLong => Status::UriTooLong,
            HttpError::HeadTooLarge | HttpError::TooManyHeaders => {
                Status::RequestHeaderFieldsTooLarge
            }
            HttpError::UnsupportedVersion => Status::HttpVersionNotSupported,
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
            HttpError::BadExpect => Status::ExpectationFailed,
        }
    }
}

impl ForwardError {
    /// A request that may not be forwarded is the client's fault.
    pub fn status(&self) -> Status {
        match self {
            ForwardError::DuplicateForwardedFor => Status::BadRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::target::TargetError;

    #[test]
    fn status_codes_and_reasons() {
        let all = [
            (Status::Ok, 200, "OK"),
            (Status::BadRequest, 400, "Bad Request"),
            (Status::MovedPermanently, 301, "Moved Permanently"),
            (Status::NotFound, 404, "Not Found"),
            (Status::MethodNotAllowed, 405, "Method Not Allowed"),
            (Status::RequestTimeout, 408, "Request Timeout"),
            (Status::ContentTooLarge, 413, "Content Too Large"),
            (Status::UriTooLong, 414, "URI Too Long"),
            (Status::ExpectationFailed, 417, "Expectation Failed"),
            (
                Status::RequestHeaderFieldsTooLarge,
                431,
                "Request Header Fields Too Large",
            ),
            (Status::NotImplemented, 501, "Not Implemented"),
            (Status::BadGateway, 502, "Bad Gateway"),
            (Status::ServiceUnavailable, 503, "Service Unavailable"),
            (Status::GatewayTimeout, 504, "Gateway Timeout"),
            (
                Status::HttpVersionNotSupported,
                505,
                "HTTP Version Not Supported",
            ),
        ];
        for (status, code, reason) in all {
            assert_eq!(status.code(), code);
            assert_eq!(status.reason(), reason);
        }
    }

    #[test]
    fn http_errors_map_to_statuses() {
        let cases = [
            (HttpError::RequestLineTooLong, Status::UriTooLong),
            (HttpError::HeadTooLarge, Status::RequestHeaderFieldsTooLarge),
            (
                HttpError::TooManyHeaders,
                Status::RequestHeaderFieldsTooLarge,
            ),
            (HttpError::BadRequestLine, Status::BadRequest),
            (HttpError::BadMethod, Status::BadRequest),
            (HttpError::BadVersion, Status::BadRequest),
            (
                HttpError::UnsupportedVersion,
                Status::HttpVersionNotSupported,
            ),
            (HttpError::BadTarget(TargetError::Empty), Status::BadRequest),
            (HttpError::BareLf, Status::BadRequest),
            (HttpError::BadHeaderName, Status::BadRequest),
            (HttpError::BadHeaderValue, Status::BadRequest),
            (HttpError::ObsFold, Status::BadRequest),
            (HttpError::MissingHost, Status::BadRequest),
            (HttpError::DuplicateHost, Status::BadRequest),
            (HttpError::BadHost, Status::BadRequest),
            (
                HttpError::ContentLengthWithTransferEncoding,
                Status::BadRequest,
            ),
            (HttpError::BadContentLength, Status::BadRequest),
            (HttpError::DuplicateContentLength, Status::BadRequest),
            (HttpError::UnsupportedTransferEncoding, Status::BadRequest),
            (HttpError::TransferEncodingInHttp10, Status::BadRequest),
            (HttpError::ConnectionNamesFraming, Status::BadRequest),
        ];
        for (err, status) in cases {
            assert_eq!(err.status(), status, "{err:?}");
        }
    }

    #[test]
    fn forward_errors_map_to_bad_request() {
        assert_eq!(
            ForwardError::DuplicateForwardedFor.status(),
            Status::BadRequest
        );
    }
}
