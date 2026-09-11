use axum::{
    Json,
    http::{StatusCode, header::WWW_AUTHENTICATE},
    response::{IntoResponse, Response},
};
use serde::Serialize;

const OCI_AUTH_CHALLENGE: &str =
    "Basic realm=\"monoengine registry\", Bearer realm=\"monoengine registry\"";

/// Errors defined by the OCI Distribution API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OciError {
    NameUnknown,
    NameInvalid,
    ManifestUnknown,
    ManifestInvalid,
    ManifestBlobUnknown,
    BlobUnknown,
    BlobUploadUnknown,
    BlobUploadInvalid,
    DigestInvalid,
    SizeInvalid,
    RangeInvalid,
    TagInvalid,
    Unauthorized,
    Denied,
    Unsupported,
    PaginationNumberInvalid,
}

impl OciError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::NameUnknown => "NAME_UNKNOWN",
            Self::NameInvalid => "NAME_INVALID",
            Self::ManifestUnknown => "MANIFEST_UNKNOWN",
            Self::ManifestInvalid => "MANIFEST_INVALID",
            Self::ManifestBlobUnknown => "MANIFEST_BLOB_UNKNOWN",
            Self::BlobUnknown => "BLOB_UNKNOWN",
            Self::BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            Self::BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            Self::DigestInvalid => "DIGEST_INVALID",
            Self::SizeInvalid => "SIZE_INVALID",
            Self::RangeInvalid => "RANGE_INVALID",
            Self::TagInvalid => "TAG_INVALID",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Denied => "DENIED",
            Self::Unsupported => "UNSUPPORTED",
            Self::PaginationNumberInvalid => "PAGINATION_NUMBER_INVALID",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::NameUnknown => "repository name not known to registry",
            Self::NameInvalid => "invalid repository name",
            Self::ManifestUnknown => "manifest unknown",
            Self::ManifestInvalid => "manifest invalid",
            Self::ManifestBlobUnknown | Self::BlobUnknown => "blob unknown to registry",
            Self::BlobUploadUnknown => "blob upload unknown to registry",
            Self::BlobUploadInvalid => "blob upload invalid",
            Self::DigestInvalid => "provided digest did not match uploaded content",
            Self::SizeInvalid => "provided length did not match content length",
            Self::RangeInvalid => "invalid content range",
            Self::TagInvalid => "manifest tag did not match URI",
            Self::Unauthorized => "authentication required",
            Self::Denied => "requested access to the resource is denied",
            Self::Unsupported => "the operation is unsupported",
            Self::PaginationNumberInvalid => "invalid number of results requested",
        }
    }

    pub const fn status(self) -> StatusCode {
        match self {
            Self::NameInvalid
            | Self::ManifestInvalid
            | Self::ManifestBlobUnknown
            | Self::DigestInvalid
            | Self::SizeInvalid
            | Self::TagInvalid
            | Self::PaginationNumberInvalid => StatusCode::BAD_REQUEST,
            Self::RangeInvalid => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::NameUnknown
            | Self::ManifestUnknown
            | Self::BlobUnknown
            | Self::BlobUploadUnknown
            | Self::BlobUploadInvalid => StatusCode::NOT_FOUND,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Denied => StatusCode::FORBIDDEN,
            Self::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
        }
    }

    pub fn with_detail(self, detail: impl Into<String>) -> OciErrorResponse {
        OciErrorResponse {
            error: self,
            detail: detail.into(),
        }
    }
}

impl IntoResponse for OciError {
    fn into_response(self) -> Response {
        self.with_detail("").into_response()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciErrorResponse {
    error: OciError,
    detail: String,
}

impl IntoResponse for OciErrorResponse {
    fn into_response(self) -> Response {
        let error = self.error;
        let mut response = (error.status(), Json(ErrorEnvelope::from(self))).into_response();
        if error == OciError::Unauthorized {
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static(OCI_AUTH_CHALLENGE),
            );
        }
        response
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    errors: [ErrorEntry; 1],
}

impl From<OciErrorResponse> for ErrorEnvelope {
    fn from(response: OciErrorResponse) -> Self {
        Self {
            errors: [ErrorEntry {
                code: response.error.code(),
                message: response.error.message(),
                detail: response.detail,
            }],
        }
    }
}

#[derive(Serialize)]
struct ErrorEntry {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    detail: String,
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes,
        http::{StatusCode, header::WWW_AUTHENTICATE},
        response::IntoResponse,
    };

    use super::OciError;

    #[tokio::test]
    async fn envelope_shape() {
        let response = OciError::NameUnknown.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("error envelope body")
                .as_ref(),
            br#"{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known to registry"}]}"#,
        );

        let response = OciError::Unauthorized
            .with_detail("expired")
            .into_response();
        assert_eq!(
            response.headers().get(WWW_AUTHENTICATE).expect("challenge"),
            "Basic realm=\"monoengine registry\", Bearer realm=\"monoengine registry\"",
        );
    }

    #[test]
    fn status_map_full_enum() {
        let cases = [
            (OciError::NameUnknown, StatusCode::NOT_FOUND),
            (OciError::NameInvalid, StatusCode::BAD_REQUEST),
            (OciError::ManifestUnknown, StatusCode::NOT_FOUND),
            (OciError::ManifestInvalid, StatusCode::BAD_REQUEST),
            (OciError::ManifestBlobUnknown, StatusCode::BAD_REQUEST),
            (OciError::BlobUnknown, StatusCode::NOT_FOUND),
            (OciError::BlobUploadUnknown, StatusCode::NOT_FOUND),
            (OciError::BlobUploadInvalid, StatusCode::NOT_FOUND),
            (OciError::DigestInvalid, StatusCode::BAD_REQUEST),
            (OciError::SizeInvalid, StatusCode::BAD_REQUEST),
            (OciError::RangeInvalid, StatusCode::RANGE_NOT_SATISFIABLE),
            (OciError::TagInvalid, StatusCode::BAD_REQUEST),
            (OciError::Unauthorized, StatusCode::UNAUTHORIZED),
            (OciError::Denied, StatusCode::FORBIDDEN),
            (OciError::Unsupported, StatusCode::METHOD_NOT_ALLOWED),
            (OciError::PaginationNumberInvalid, StatusCode::BAD_REQUEST),
        ];

        for (error, status) in cases {
            assert_eq!(error.status(), status, "{}", error.code());
        }
    }
}
