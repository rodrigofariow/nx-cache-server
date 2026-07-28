use crate::domain::storage::{StorageError, StorageProvider};
use crate::server::{error::ServerError, validation, AppState};
use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

pub async fn store_artifact<T: StorageProvider>(
    Path(hash): Path<String>,
    State(state): State<AppState<T>>,
    body: Body,
) -> Result<impl IntoResponse, ServerError> {
    validation::validate_hash(&hash)?;

    // The Nx client only ever sends the opaque task hash, never the command that
    // produced it - the hash is the only identifier we can log here.
    match state.storage.exists(&hash).await {
        Ok(true) => {
            tracing::info!("cache STORE skipped (already cached): {hash}");
            // Drain the still-arriving request body before responding. Answering
            // while the client is mid-upload makes the kernel reset the connection
            // once hyper drops the unread body, and the Nx client then reports
            // "Failed to send request" instead of seeing the 409 - large artifacts
            // would never be storable.
            drain_body(body).await;
            return Err(StorageError::AlreadyExists.into());
        }
        Ok(false) => {}
        // Storage backend unreachable / denied / throttled. Nx aborts the *entire*
        // command on any store response other than 200 ("Misconfigured remote cache
        // endpoint: Unexpected response status") / 409 / 403, so we never surface a
        // 5xx: degrade to a 200 no-op (artifact NOT cached; a later run misses and
        // re-runs). Logged at error! so it stays visible even at --log-level error.
        Err(e) => {
            tracing::error!("cache STORE degraded to no-op: existence check for {hash} failed: {e}; returning 200 (artifact NOT cached) so Nx does not abort the build");
            drain_body(body).await;
            return Ok((StatusCode::OK, ""));
        }
    }

    // For now, let's use a simpler approach - collect the body into bytes
    // TODO: Implement true streaming later for better memory efficiency
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| ServerError::BadRequest)?;

    let size = bytes.len();
    let cursor = std::io::Cursor::new(bytes);
    let reader_stream = tokio_util::io::ReaderStream::new(cursor);

    match state.storage.store(&hash, reader_stream).await {
        // The Nx client treats only 200 as a successful store; any other 2xx (202
        // included) is "Unexpected response status", which makes Nx re-upload every
        // artifact until its 6 retries are exhausted and then abort the whole build.
        Ok(()) => {
            tracing::info!("cache STORE: {hash} ({})", human_size(size));
            Ok((StatusCode::OK, ""))
        }
        // Raced with a concurrent PUT between our exists() check and the store.
        Err(StorageError::AlreadyExists) => {
            tracing::info!("cache STORE skipped (already cached): {hash}");
            Err(StorageError::AlreadyExists.into())
        }
        // Same degradation rationale as the existence check above.
        Err(e) => {
            tracing::error!("cache STORE degraded to no-op: storing {hash} ({}) failed: {e}; returning 200 (artifact NOT cached) so Nx does not abort the build", human_size(size));
            Ok((StatusCode::OK, ""))
        }
    }
}

/// Read and discard the rest of a request body, so a response sent before the
/// client finished uploading never turns into a connection reset on their side.
/// Errors are ignored - the client may already have hung up, and the response
/// we are about to send is the same either way.
async fn drain_body(body: Body) {
    let mut stream = body.into_data_stream();
    // Stop at the first error too: an `Err` item is terminal, and polling a body
    // again after that is unspecified (it may panic or loop).
    while let Some(Ok(_)) = tokio_stream::StreamExt::next(&mut stream).await {}
}

pub async fn retrieve_artifact<T: StorageProvider>(
    Path(hash): Path<String>,
    State(state): State<AppState<T>>,
) -> Result<impl IntoResponse, ServerError> {
    validation::validate_hash(&hash)?;

    // The hash is the only identifier the Nx client sends - it never includes
    // the command/target that produced the artifact, so that is all we can log.
    let reader = match state.storage.retrieve(&hash).await {
        Ok(reader) => {
            tracing::info!("cache HIT: {hash}");
            reader
        }
        Err(StorageError::NotFound) => {
            tracing::info!("cache MISS: {hash}");
            return Err(StorageError::NotFound.into());
        }
        // Storage backend unreachable / denied / throttled. Nx aborts the *entire*
        // command on any non-200/404 from the cache, so we degrade to a cache MISS
        // (404) and let Nx run the task locally instead of failing the build. The 404
        // is indistinguishable from a genuine miss to the client by design. Logged at
        // error! so it stays visible even at --log-level error.
        Err(e) => {
            tracing::error!("cache MISS forced (storage degraded): GET {hash} failed: {e}; returning 404 so Nx runs the task locally instead of failing the build");
            return Err(StorageError::NotFound.into());
        }
    };

    let stream = tokio_util::io::ReaderStream::new(reader);
    let body = Body::from_stream(stream);

    Ok((
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        body,
    ))
}

pub async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Format a byte count as a short, human-readable string (e.g. `24.5 KB`).
fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
