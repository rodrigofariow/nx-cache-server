# CLAUDE.md

Guidance for AI agents working in this repository.

## Project Overview

`nx-cache-server` is a self-hosted [Nx custom remote cache](https://nx.dev/recipes/running-tasks/self-hosted-caching) server written in Rust. It implements the Nx remote-cache HTTP contract and streams build artifacts to/from cloud object storage. The only backend shipped today is **AWS S3** (and S3-compatible services like MinIO), built as the `nx-cache-aws` binary.

- Single self-contained executable, no runtime deps.
- Stack: `axum` (HTTP), `aws-sdk-s3` / `aws-config` (storage), `clap` (config), `tracing` (logging), `tokio` (runtime).
- This checkout is a fork: `origin` → `git@github.com:rodrigofariow/nx-cache-server.git`, `upstream` → `git@github.com:nxcite/nx-cache-server.git`.

## Build / Run / Test

```bash
# Build (debug)
cargo build

# Release build for the published Linux x86_64 target (matches the release workflow)
cargo build --release --bin nx-cache-aws --target x86_64-unknown-linux-gnu
# → target/x86_64-unknown-linux-gnu/release/nx-cache-aws

# Run (config via flags or env — see README)
cargo run --bin nx-cache-aws -- \
  --bucket-name my-bucket --region eu-north-1 \
  --service-access-token <token> --log-level debug

# Lint / format / test
cargo clippy --all-targets
cargo fmt
cargo test
```

No `rust-toolchain.toml` is pinned; the release CI uses `rustup update stable`. Release builds are cut **manually** via the `Release` GitHub Action (`workflow_dispatch`, input = version tag like `v1.2.0`), which cross-compiles the 5 targets in `.github/workflows/release.yml` and publishes a GitHub Release.

### Release target compatibility — primary Linux build MUST run on Debian Bookworm

The published primary Linux artifact is **`x86_64-unknown-linux-gnu`** (`nx-cache-aws-<ver>-linux-x86_64`). It is deployed into / run alongside the **`node:24-slim`** image, which is based on **Debian Bookworm (Debian 12, glibc 2.36)**. **This binary MUST NOT require a glibc newer than 2.36**, or it dies at startup with `version 'GLIBC_2.3x' not found`.

glibc is **backward**-compatible only: a binary built against an *older* glibc runs on newer systems, **never the reverse**. So the release binary must be **built against glibc ≤ 2.36**, and it must stay **as optimized as the other targets** (same `--release` profile / LTO — do not trade optimization for compatibility).

- ⚠️ **Current gap:** the workflow builds this target on `ubuntu-latest` (glibc **2.39**) → the artifact will *not* run on Bookworm. Building on a dev machine (newer glibc still) is worse — never `cp` a locally-built binary into a Bookworm container expecting it to run.
- **Fixes (any one), all preserving full release optimization:** build inside a `debian:bookworm`/`rust:*-bookworm` container; **or** pin that matrix leg to `ubuntu-22.04` (glibc 2.35 ≤ 2.36); **or** use `cargo-zigbuild` with `--target x86_64-unknown-linux-gnu.2.36`.
- `x86_64-unknown-linux-musl` (fully static, no glibc dep) is an option **only if benchmarked to be as fast** — musl's allocator can regress throughput, and the bar is "as optimized as the other targets".

## Architecture

Clean three-layer split under `src/`:

- **`domain/`** — storage-agnostic core.
  - `storage.rs`: the `StorageProvider` trait (`exists` / `store` / `retrieve`) and `StorageError` (`NotFound` / `AlreadyExists` / `OperationFailed(String)` — the string carries the backend-specific cause for logging). This is the seam any new backend implements.
  - `config.rs`: `ServerConfig` (clap), `ConfigValidator` trait, `ConfigError` (with rich user-facing messages), and `LogLevel`.
- **`infra/`** — concrete backends. `aws.rs` holds `AwsStorageConfig` (clap) + `S3Storage` implementing `StorageProvider` over `aws-sdk-s3`.
- **`server/`** — HTTP layer.
  - `mod.rs`: router + `AppState<T>` + `run_server`.
  - `handlers.rs`: the three endpoints.
  - `middleware.rs`: bearer-token auth (constant-time compare via `subtle`).
  - `validation.rs`: hash key validation (alphanumeric/`-`/`_`, ≤128 chars).
  - `error.rs`: `ServerError` → HTTP mapping.
- **`bin/aws.rs`** — wires `ServerConfig` + `AwsStorageConfig` into the `nx-cache-aws` clap CLI, initializes logging, then `run_server`.

The server is generic over `T: StorageProvider`, so adding a backend (GCS/Azure/etc.) means: a new `infra/<backend>.rs` implementing the trait, a new `src/bin/<backend>.rs`, and a `[[bin]]` entry in `Cargo.toml`.

### HTTP API (Nx contract)

| Method | Route | Auth | Behavior |
|---|---|---|---|
| `GET`  | `/health` | none | `200 "OK"` |
| `GET`  | `/v1/cache/{hash}` | Bearer | `200` + `application/octet-stream` body on hit; `404` on miss **or any storage-backend failure (degraded — see below)** |
| `PUT`  | `/v1/cache/{hash}` | Bearer | `202` on store **(or on any storage-backend failure — degraded no-op)**; `409` if the key already exists (cache entries are immutable — never overwritten) |

Auth failures return `401`. Nx clients connect via `NX_SELF_HOSTED_REMOTE_CACHE_SERVER` + `NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN` (must equal the server's `--service-access-token`).

**Graceful degradation — never surface `5xx` to Nx.** Nx aborts the *entire* command on any unexpected status from the cache: a `500` makes it fail with *"Misconfigured remote cache endpoint: Unexpected response status"* **even though the wrapped task itself succeeded**. So `handlers.rs` never returns `5xx` for a storage-backend failure (S3 unreachable / `AccessDenied` / throttled / billing / quota): `GET` degrades to a `404` cache MISS (Nx runs the task locally) and `PUT` degrades to a `202` no-op (the artifact is simply not cached; a later run misses and re-runs). The real cause is logged at `error!` (see Logging). **Trade-off:** a genuinely misconfigured backend (wrong bucket, missing IAM perms, bad creds) is now an *invisible* permanent cache miss to clients — the only signal is the server's `error!` log, so **alert on those logs**. (There is no strict/fail-loud mode flag today; if one is wanted it goes on `ServerConfig`.)

**Bind address:** the server binds `--host` / `HOST`, defaulting to `127.0.0.1` (loopback only — not reachable over the network). This is the safe default for the local-per-dev model. Set `--host 0.0.0.0` only for a central/shared deployment, and only behind a TLS-terminating reverse proxy (the server speaks plain HTTP).

## Logging — important

`tracing-subscriber` is compiled **without the `env-filter` feature** (`Cargo.toml`: `default-features = false, features = ["fmt"]`). Therefore:

- **`RUST_LOG` is inert.** It does nothing. Do not suggest it.
- Verbosity is controlled by `--log-level <trace|debug|info|warn|error>` / `LOG_LEVEL`, or the `--debug` shorthand (= `debug`). Resolution order lives in `bin/aws.rs`: `--log-level` wins, else `--debug`, else `info`. Implemented via `tracing_subscriber::fmt().with_max_level(...)`.

### Cache hit/miss logging

`server/handlers.rs` logs one human-readable line per cache operation at `info!`:

- `cache HIT: <hash>` — `GET` found the artifact
- `cache MISS: <hash>` — `GET` returned 404
- `cache STORE: <hash> (24.5 KB)` — `PUT` stored a new artifact
- `cache STORE skipped (already cached): <hash>` — `PUT` hit the 409 immutability path

Because these are `info!` events and the subscriber uses `with_max_level`, they show at `info`/`debug`/`trace` and are hidden at `warn`/`error`.

Storage-backend failures that get **degraded** (see "Graceful degradation") are logged at **`error!`** instead — so they remain visible at *every* level, including `--log-level error` — and carry the underlying cause (`StorageError::OperationFailed`'s string, built from the S3 error via `aws_sdk_s3::error::DisplayErrorContext`, which walks the source chain so even a timeout/`Connection refused` shows up):

- `cache MISS forced (storage degraded): GET <hash> failed: <cause>; returning 404 …` — `GET` storage failure
- `cache STORE degraded to no-op: … <hash> … failed: <cause>; returning 202 …` — `PUT` storage failure (existence check or the store itself)

**The `<hash>` is all the server can log — it is *not* the command.** The Nx client only ever sends the opaque task hash in the URL (`/v1/cache/{hash}`); there is no header or body field carrying the target/command. To correlate a hash back to a command you must look client-side (e.g. `NX_VERBOSE_LOGGING=true`). There is no request-logging middleware (e.g. `tower-http::trace`) wired in.

## Gotchas & known rough edges

- **S3 + missing `s3:ListBucket` IAM permission:** S3 returns `403 AccessDenied` (not `404 NoSuchKey`) for `GetObject`/`HeadObject` on a non-existent key when the caller lacks `s3:ListBucket`. `S3Storage` only maps `NoSuchKey`/`NotFound` → `StorageError::NotFound`; everything else → `OperationFailed`. **With graceful degradation this no longer 500s** — instead every lookup degrades to a `404` miss and every store to a `202` no-op, so the cache *silently never works* (permanent misses, nothing ever stored). The symptom moved from "500 errors" to "caching appears to do nothing"; the `error!` log shows the `403 AccessDenied`. Fix: grant `s3:ListBucket` on the **bucket** ARN (`arn:aws:s3:::bucket`, no `/*`) in addition to `s3:GetObject`/`s3:PutObject` on the **object** ARN (`arn:aws:s3:::bucket/*`).
- **401 must carry a `text/plain` body (handled — don't regress):** Nx rejects a bodyless 401 with *"Misconfigured remote cache endpoint: Requests should respond with text/plain on 401s."* `auth_middleware` therefore returns `ServerError::Unauthorized` (which sets `text/plain` via `error.rs`), **not** a bare `Err(StatusCode::UNAUTHORIZED)` — don't revert it to a bare status. Pinned by the `missing_token_is_unauthorized` / `wrong_token_is_unauthorized` contract tests.
- **Not actually streaming yet:** despite the README's streaming claims, `store` buffers the whole body into a `Vec<u8>` before the S3 `put_object` (see `TODO`s in `handlers.rs` / `infra/aws.rs`). Retrieve does stream.
- **Nx never caches failed tasks.** If a wrapped task exits non-zero, Nx writes nothing to the remote cache — so "nothing in the cache" can be a failing build, not a cache bug.
- **Immutable entries:** `store` checks `exists` first and returns `409`; there is no overwrite path.

## Conventions

- Conventional Commits. End commit messages with the trailer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- Keep the layer boundaries: domain has no AWS/axum imports; backends depend only on the `domain` traits; HTTP concerns stay in `server/`.
- New config flags go on the relevant clap struct (`ServerConfig` or `AwsStorageConfig`) with both a `long` flag and an `env`, and should be documented in `README.md`.

### Rust rules (apply to every change)

Non-negotiables distilled from the `rust-best-practices` skill (`.agents/skills/rust-best-practices/` — read it for the full rationale and examples):

- **No `unwrap()`/`expect()` outside tests.** Return `Result<T, E>` for fallible ops and propagate with `?`. Use `let … else { return Err(…) }` for expected-absence early returns; `inspect_err`/`map_err` to log-then-transform.
- **Errors:** `thiserror` for the domain/library error enums (`StorageError`, `ConfigError`, `ServerError`); `anyhow` only in `src/bin/`. Wrap nested errors with `#[from]`.
- **Borrow over clone.** Take `&str` / `&[T]` / `&T` in params; never `.clone()` in a loop. Derive `Copy` only on small (≤24 B) plain-data types with no heap fields.
- **Comments explain *why*, not *what*;** `///` doc comments for public APIs. `#[expect(clippy::…)]` with a justifying comment over `#[allow(…)]`.
- **Gate before done:** `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean. Import order: `std` → external → workspace → `crate`/`super`.
