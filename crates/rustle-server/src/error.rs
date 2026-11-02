use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rustle_types::{ApiError, ApiErrorBody};

/// Error returned by API handlers and services.
#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }
    pub fn too_many(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
    }
    pub fn unprocessable(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }
    pub fn internal(err: impl std::fmt::Display) -> Self {
        // Details go to the log, never to the client.
        tracing::error!("internal error: {err}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            if f.code == rusqlite::ErrorCode::ConstraintViolation {
                return AppError::conflict("already exists or violates a constraint");
            }
        }
        AppError::internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ApiError {
            error: ApiErrorBody {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_status_mapping() {
        assert_eq!(AppError::conflict("busy").to_string(), "conflict: busy");
        assert_eq!(AppError::not_found("x").to_string(), "not_found: x");
        for (e, s) in [
            (AppError::bad_request("a"), StatusCode::BAD_REQUEST),
            (AppError::unauthorized("a"), StatusCode::UNAUTHORIZED),
            (AppError::forbidden("a"), StatusCode::FORBIDDEN),
            (AppError::not_found("a"), StatusCode::NOT_FOUND),
            (AppError::conflict("a"), StatusCode::CONFLICT),
            (AppError::too_many("a"), StatusCode::TOO_MANY_REQUESTS),
            (AppError::unavailable("a"), StatusCode::SERVICE_UNAVAILABLE),
            (
                AppError::unprocessable("c", "a"),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                AppError::internal("boom"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(e.status, s);
        }
    }

    #[test]
    fn constraint_violations_become_conflicts_and_other_db_errors_are_internal() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute("CREATE TABLE t(a UNIQUE)", []).unwrap();
        db.execute("INSERT INTO t VALUES(1)", []).unwrap();
        let dup = db.execute("INSERT INTO t VALUES(1)", []).unwrap_err();
        let e: AppError = dup.into();
        assert_eq!(e.status, StatusCode::CONFLICT);
        let missing = db.execute("INSERT INTO nope VALUES(1)", []).unwrap_err();
        let e: AppError = missing.into();
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !e.message.contains("nope"),
            "internal details are not leaked: {}",
            e.message
        );
    }
}
