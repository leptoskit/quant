use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use quant_access::Denied;
use quant_api::ApiError;
use quant_store::StoreError;

/// An API failure.
#[derive(Debug)]
pub enum Error {
	/// No valid signature or session.
	Unauthenticated(String),
	/// Signed, but not allowed.
	Forbidden(Denied),
	/// Nothing there, or nothing the caller may know is there.
	NotFound,
	/// The request was malformed.
	Invalid(String),
	/// Owner unlock is throttled after failures.
	Locked(u64),
	/// Something broke.
	Internal(String),
}

impl From<StoreError> for Error {
	fn from(error: StoreError) -> Self {
		match error {
			StoreError::NotFound => Error::NotFound,
			StoreError::Invalid(message) => Error::Invalid(message),
			other => Error::Internal(other.to_string()),
		}
	}
}

impl From<Denied> for Error {
	fn from(denied: Denied) -> Self {
		Error::Forbidden(denied)
	}
}

impl From<quant_access::AccessError> for Error {
	fn from(error: quant_access::AccessError) -> Self {
		match error {
			quant_access::AccessError::Unauthenticated(why) => Error::Unauthenticated(why),
			other => Error::Invalid(other.to_string()),
		}
	}
}

impl IntoResponse for Error {
	fn into_response(self) -> Response {
		let (status, code, message) = match self {
			Error::Unauthenticated(why) => (StatusCode::UNAUTHORIZED, "unauthenticated", why),
			Error::Forbidden(Denied::RateLimited) => {
				(
					StatusCode::TOO_MANY_REQUESTS,
					"rate_limited",
					Denied::RateLimited.to_string(),
				)
			}
			Error::Forbidden(denied) => (StatusCode::FORBIDDEN, "forbidden", denied.to_string()),
			Error::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".to_owned()),
			Error::Invalid(why) => (StatusCode::BAD_REQUEST, "invalid", why),
			Error::Locked(seconds) => {
				(
					StatusCode::TOO_MANY_REQUESTS,
					"locked",
					format!("too many wrong passphrases; try again in {seconds}s"),
				)
			}
			Error::Internal(why) => {
				tracing::error!(%why, "internal error");
				(
					StatusCode::INTERNAL_SERVER_ERROR,
					"internal",
					"internal error".to_owned(),
				)
			}
		};
		(
			status,
			Json(ApiError {
				code: code.to_owned(),
				message,
			}),
		)
			.into_response()
	}
}

/// A handler result.
pub type ApiResult<T> = Result<T, Error>;
