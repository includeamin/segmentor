//! HTTP error type and its mapping to responses.
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, RETRY_AFTER};
use axum::response::{IntoResponse, Response};

use crate::error::Error;
use crate::registry::RegistryError;

const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

pub(crate) type HttpResult<T> = std::result::Result<T, HttpError>;

#[derive(Debug)]
pub(crate) struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    pub(crate) fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.to_owned(),
        }
    }

    pub(crate) fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }

    pub(crate) fn bad_gateway(message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message,
        }
    }

    pub(crate) fn unavailable(message: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.to_owned(),
        }
    }
}

impl From<RegistryError> for HttpError {
    fn from(error: RegistryError) -> Self {
        match error {
            RegistryError::NotFound => Self::not_found("asset does not exist"),
            RegistryError::Unavailable(message) => Self::unavailable(&message),
            RegistryError::BadUpstream(message) => Self::bad_gateway(message),
            RegistryError::LoadFailed(message) => Self::internal(message),
        }
    }
}

impl From<Error> for HttpError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(message) => Self::not_found(message),
            Error::Upstream(message) | Error::LocationRejected(message) => {
                Self::bad_gateway(message)
            }
            Error::UpstreamUnavailable(message) => Self::unavailable(&message),
            error => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let public_message = if self.status.is_server_error() {
            if self.status == StatusCode::SERVICE_UNAVAILABLE {
                tracing::warn!(
                    event = "request_shed",
                    http.status = self.status.as_u16(),
                    error = %self.message,
                );
                "service unavailable"
            } else if self.status == StatusCode::BAD_GATEWAY {
                tracing::error!(
                    event = "upstream_failed",
                    http.status = self.status.as_u16(),
                    error = %self.message,
                );
                "bad gateway"
            } else {
                tracing::error!(
                    event = "request_failed",
                    http.status = self.status.as_u16(),
                    error = %self.message,
                );
                "internal server error"
            }
        } else {
            tracing::warn!(
                event = "request_rejected",
                http.status = self.status.as_u16(),
                error = %self.message,
            );
            &self.message
        };
        let mut response = (self.status, public_message.to_owned()).into_response();
        response.headers_mut().insert(CACHE_CONTROL, NO_STORE);
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}
