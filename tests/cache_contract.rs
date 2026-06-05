//! HTTP contract tests for the Nx remote-cache endpoints.
//!
//! These pin the status codes Nx depends on — in particular the **graceful
//! degradation** rules: a storage-backend failure must never surface as a 5xx
//! (which makes Nx abort the whole command). Instead `GET` degrades to a 404
//! cache miss and `PUT` degrades to a 202 no-op. The handlers are generic over
//! `StorageProvider`, so everything here runs against an in-process mock — no
//! AWS, no network, no secrets.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use nx_cache_server::domain::config::ServerConfig;
use nx_cache_server::domain::storage::{StorageError, StorageProvider};
use nx_cache_server::server::{create_router, AppState};
use tokio::io::AsyncRead;
use tokio_util::io::ReaderStream;
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "test-secret-token";
const HIT_PAYLOAD: &[u8] = b"cached-artifact-bytes";

/// A `StorageProvider` whose three operations each return a scripted outcome,
/// so a test can drive any single code path through the handlers.
#[derive(Clone, Copy)]
struct MockStorage {
    exists: ExistsResult,
    store: StoreResult,
    retrieve: RetrieveResult,
}

#[derive(Clone, Copy)]
enum ExistsResult {
    Present,
    Absent,
    Fail,
}

#[derive(Clone, Copy)]
enum StoreResult {
    Stored,
    Exists,
    Fail,
}

#[derive(Clone, Copy)]
enum RetrieveResult {
    Hit,
    Miss,
    Fail,
}

impl MockStorage {
    fn new(exists: ExistsResult, store: StoreResult, retrieve: RetrieveResult) -> Self {
        Self {
            exists,
            store,
            retrieve,
        }
    }
}

#[async_trait]
impl StorageProvider for MockStorage {
    async fn exists(&self, _hash: &str) -> Result<bool, StorageError> {
        match self.exists {
            ExistsResult::Present => Ok(true),
            ExistsResult::Absent => Ok(false),
            ExistsResult::Fail => Err(StorageError::OperationFailed("mock exists failure".into())),
        }
    }

    async fn store(
        &self,
        _hash: &str,
        _data: ReaderStream<impl AsyncRead + Send + Unpin>,
    ) -> Result<(), StorageError> {
        match self.store {
            StoreResult::Stored => Ok(()),
            StoreResult::Exists => Err(StorageError::AlreadyExists),
            StoreResult::Fail => Err(StorageError::OperationFailed("mock store failure".into())),
        }
    }

    async fn retrieve(
        &self,
        _hash: &str,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, StorageError> {
        match self.retrieve {
            RetrieveResult::Hit => Ok(Box::new(HIT_PAYLOAD)),
            RetrieveResult::Miss => Err(StorageError::NotFound),
            RetrieveResult::Fail => Err(StorageError::OperationFailed(
                "mock retrieve failure".into(),
            )),
        }
    }
}

fn test_config() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 3000,
        service_access_token: TOKEN.to_string(),
        debug: false,
        log_level: None,
    }
}

fn app(mock: MockStorage) -> Router {
    let state = AppState {
        storage: Arc::new(mock),
        config: Arc::new(test_config()),
    };
    create_router(&state).with_state(state)
}

fn get(hash: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("/v1/cache/{hash}"));
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn put(hash: &str, token: Option<&str>, body: &[u8]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(format!("/v1/cache/{hash}"));
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::from(body.to_vec())).unwrap()
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

// --- /health (public) -------------------------------------------------------

#[tokio::test]
async fn health_is_public_and_returns_ok() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Miss,
    ));
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let res = app.oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_bytes(res).await, b"OK");
}

// --- auth -------------------------------------------------------------------

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Hit,
    ));
    let res = app.oneshot(get("abc123", None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Hit,
    ));
    let res = app
        .oneshot(get("abc123", Some("not-the-token")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// --- hash validation --------------------------------------------------------

#[tokio::test]
async fn invalid_hash_is_bad_request() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Hit,
    ));
    // '.' is outside the allowed [alphanumeric-_] set.
    let res = app.oneshot(get("bad.hash", Some(TOKEN))).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

// --- GET /v1/cache/{hash} ---------------------------------------------------

#[tokio::test]
async fn get_hit_returns_200_with_octet_stream_body() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Hit,
    ));
    let res = app.oneshot(get("abc123", Some(TOKEN))).await.unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    assert_eq!(body_bytes(res).await, HIT_PAYLOAD);
}

#[tokio::test]
async fn get_genuine_miss_returns_404() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Miss,
    ));
    let res = app.oneshot(get("abc123", Some(TOKEN))).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_storage_failure_degrades_to_404() {
    // Backend error must NOT become a 5xx — Nx would abort the build.
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Fail,
    ));
    let res = app.oneshot(get("abc123", Some(TOKEN))).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// --- PUT /v1/cache/{hash} ---------------------------------------------------

#[tokio::test]
async fn put_new_artifact_returns_202() {
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Stored,
        RetrieveResult::Miss,
    ));
    let res = app
        .oneshot(put("abc123", Some(TOKEN), b"payload"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn put_existing_artifact_returns_409() {
    let app = app(MockStorage::new(
        ExistsResult::Present,
        StoreResult::Stored,
        RetrieveResult::Miss,
    ));
    let res = app
        .oneshot(put("abc123", Some(TOKEN), b"payload"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_store_race_already_exists_returns_409() {
    // exists() said "absent" but store() lost a race and reports AlreadyExists.
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Exists,
        RetrieveResult::Miss,
    ));
    let res = app
        .oneshot(put("abc123", Some(TOKEN), b"payload"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_exists_check_failure_degrades_to_202() {
    // Backend error during the existence check must degrade to a no-op 202.
    let app = app(MockStorage::new(
        ExistsResult::Fail,
        StoreResult::Stored,
        RetrieveResult::Miss,
    ));
    let res = app
        .oneshot(put("abc123", Some(TOKEN), b"payload"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn put_store_failure_degrades_to_202() {
    // Backend error during the store itself must degrade to a no-op 202.
    let app = app(MockStorage::new(
        ExistsResult::Absent,
        StoreResult::Fail,
        RetrieveResult::Miss,
    ));
    let res = app
        .oneshot(put("abc123", Some(TOKEN), b"payload"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}
