//! Status codes the server can emit, and the mapping from parse errors.

use super::request::HttpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    BadRequest,
    NotFound,
    RequestTimeout,
    UriTooLong,
    RequestHeaderFieldsTooLarge,
    NotImplemented,
    ServiceUnavailable,
    HttpVersionNotSupported,
}

impl Status {
    pub fn code(self) -> u16 {
        match self {
            Status::BadRequest => 400,
            Status::NotFound => 404,
            Status::RequestTimeout => 408,
            Status::UriTooLong => 414,
            Status::RequestHeaderFieldsTooLarge => 431,
            Status::NotImplemented => 501,
            Status::ServiceUnavailable => 503,
            Status::HttpVersionNotSupported => 505,
        }
    }

    pub fn reason(self) -> &'static str {
        match self {
            Status::BadRequest => "Bad Request",
            Status::NotFound => "Not Found",
            Status::RequestTimeout => "Request Timeout",
            Status::UriTooLong => "URI Too Long",
            Status::RequestHeaderFieldsTooLarge => "Request Header Fields Too Large",
            Status::NotImplemented => "Not Implemented",
            Status::ServiceUnavailable => "Service Unavailable",
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
            | HttpError::TransferEncodingInHttp10 => Status::BadRequest,
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
            (Status::BadRequest, 400, "Bad Request"),
            (Status::NotFound, 404, "Not Found"),
            (Status::RequestTimeout, 408, "Request Timeout"),
            (Status::UriTooLong, 414, "URI Too Long"),
            (
                Status::RequestHeaderFieldsTooLarge,
                431,
                "Request Header Fields Too Large",
            ),
            (Status::NotImplemented, 501, "Not Implemented"),
            (Status::ServiceUnavailable, 503, "Service Unavailable"),
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
        ];
        for (err, status) in cases {
            assert_eq!(err.status(), status, "{err:?}");
        }
    }
}
