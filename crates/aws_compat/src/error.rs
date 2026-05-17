use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

/// Errors that an S3 Vectors (restJson1) handler can return on the wire.
///
/// Mapped to the `x-amzn-errortype` header + lowercase-`message` body
/// shape that the AWS service uses (captured live in `CLAUDE.md` C-1).
/// The SDK's `aws_sdk_s3vectors::types::error::*` enum keys off the
/// header value, so the strings here are the contract.
#[derive(Debug, Error)]
pub enum AwsError {
    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Validation(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{message}")]
    Internal { message: String },
}

impl AwsError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_type(&self) -> &'static str {
        match self {
            Self::Conflict(_) => "ConflictException",
            Self::Validation(_) => "ValidationException",
            Self::NotFound(_) => "NotFoundException",
            Self::Internal { .. } => "InternalServerException",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Conflict(m) | Self::Validation(m) | Self::NotFound(m) => m,
            Self::Internal { message } => message,
        }
    }
}

/// Serializable error body — note lowercase `message` per AWS wire shape.
#[derive(Serialize)]
pub struct RestJsonError<'a> {
    pub message: &'a str,
}

impl IntoResponse for AwsError {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        // x-amzn-errortype is how the AWS SDK identifies the variant.
        // Must be an ASCII header value; the enum's `error_type` is
        // always &'static str so unwrap is safe.
        headers.insert(
            "x-amzn-errortype",
            HeaderValue::from_static(self.error_type()),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let body = Json(RestJsonError {
            message: self.message(),
        });

        (self.status(), headers, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn conflict_maps_to_409_and_correct_headers() {
        let err = AwsError::Conflict("nope".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            resp.headers().get("x-amzn-errortype").unwrap(),
            "ConflictException"
        );
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        // Body uses lowercase `message` — this is the AWS wire shape and
        // must not regress (CLAUDE.md C-1).
        assert_eq!(&bytes[..], br#"{"message":"nope"}"#);
    }

    #[tokio::test]
    async fn validation_maps_to_400() {
        let resp = AwsError::Validation("bad input".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get("x-amzn-errortype").unwrap(),
            "ValidationException"
        );
    }
}
