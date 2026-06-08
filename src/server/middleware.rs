use crate::domain::storage::StorageProvider;
use crate::server::{error::ServerError, AppState};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

pub async fn auth_middleware<T>(
    State(state): State<AppState<T>>,
    request: Request,
    next: Next,
) -> Result<Response, ServerError>
where
    T: StorageProvider,
{
    // Extract Bearer token from Authorization header
    let token = request
        .headers()
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Return ServerError::Unauthorized (not a bare StatusCode) so the 401 carries
    // a text/plain body: Nx rejects a bodyless 401 with "Misconfigured remote cache
    // endpoint: Requests should respond with text/plain on 401s."
    let Some(token) = token else {
        return Err(ServerError::Unauthorized);
    };

    // Constant-time comparison for security
    if !bool::from(
        token
            .as_bytes()
            .ct_eq(state.config.service_access_token.as_bytes()),
    ) {
        return Err(ServerError::Unauthorized);
    }

    Ok(next.run(request).await)
}
