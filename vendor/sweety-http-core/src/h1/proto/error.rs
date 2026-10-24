use httparse::Error as HttparseError;

#[derive(Debug)]
pub enum ProtoError {
    HeaderName,
    HeaderValue,
    HeaderTooLarge,
    UnsupportedTransferCoding,
    /// RFC 9110 §10.1.1：无法满足的 Expect 期望值 → 417
    ExpectationFailed,
    // request-target exceeds read buffer limit (RFC 9110 §15.5.15 → 414).
    UriTooLong,
    Method,
    Uri,
    NewLine,
    Status,
    Token,
    Version,
}

impl From<HttparseError> for ProtoError {
    fn from(e: HttparseError) -> Self {
        match e {
            HttparseError::HeaderName => Self::HeaderName,
            HttparseError::HeaderValue => Self::HeaderValue,
            // Too many headers would be treated the same as header too large error.
            HttparseError::TooManyHeaders => Self::HeaderTooLarge,
            HttparseError::NewLine => Self::NewLine,
            HttparseError::Status => Self::Status,
            HttparseError::Token => Self::Token,
            HttparseError::Version => Self::Version,
        }
    }
}

impl From<http::method::InvalidMethod> for ProtoError {
    fn from(_: http::method::InvalidMethod) -> Self {
        Self::Method
    }
}

impl From<http::uri::InvalidUri> for ProtoError {
    fn from(_: http::uri::InvalidUri) -> Self {
        Self::Uri
    }
}

impl From<http::status::InvalidStatusCode> for ProtoError {
    fn from(_: http::status::InvalidStatusCode) -> Self {
        Self::Status
    }
}
