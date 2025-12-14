# Copilot / AI Agent Instructions for neptune-core

Quick orientation and actionable rules for code changes, tests, and features.

- **Big picture:** `neptune-core` is a single Rust crate (binary `neptune-core`) implementing a Layer-1 node. Core subsystems live under `src/`: networking & peers (`application::loops`, peer RPC), node state (`state` / `GlobalState`), consensus & protocol (`protocol`), RPC/API layers (`application/json_rpc`, `api`). See [src/lib.rs](src/lib.rs#L1) and [src/main.rs](src/main.rs#L1).

- **Where to make changes:**
  - Public consumer-facing APIs belong in `src/api` (used by integration tests). See [src/api/README.md](src/api/README.md#L1).
  - JSON-RPC additions follow the pattern in `src/application/json_rpc/README.md` — add an op, message types, and implement server `_call` handlers.

- **Build & feature flags:**
  - Default build: `cargo build` (binary named `neptune-core`). See [Cargo.toml](Cargo.toml#L1).
  - Some debug features require nightly, e.g. `log-slow-write-lock` / `log-slow-read-lock`: build with `cargo +nightly build --features log-slow-write-lock`.
  - `tokio-console` support is opt-in (`--features tokio-console`). The CLI flag `--tokio-console` only works when crate compiled with that feature.

- **Testing & benches:**
  - Unit & integration tests use `cargo test`. Integration tests produce useful tracing only when `NOCAPTURE=1` is set: `NOCAPTURE=1 cargo test -- --nocapture` (see [tests/README.md](tests/README.md#L1)).
  - Many tests use a regtest mode (mock proofs) — check `src/api/regtest` and integration tests for examples.
  - Benchmarks are defined in `benchmark/benches` and often set `harness = false` in `Cargo.toml`; use `cargo bench` and enable features like `arbitrary-impls` when required.

- **Concurrency & runtime:**
  - The application is `tokio`-based. The main runtime is created in `src/main.rs`. Spawned tasks communicate via channels and broadcasts (see `application::loops::channel` types and `MainLoopHandler` in `src/lib.rs`). When debugging concurrency, start by inspecting those channel types and the `initialize` function.

- **RPC / Integration patterns:**
  - RPC layer is implemented with `tarpc` and a JSON/HTTP RPC server. New RPC endpoints require: (1) new op in `json_rpc::core::api::ops::RpcApiOps`, (2) request/response structs in `json_rpc::core::model::message`, (3) RPC trait method and `_call` server implementation. See [src/application/json_rpc/README.md](src/application/json_rpc/README.md#L1).

- **Project-specific conventions:**
  - Many modules are deliberately `pub` and re-exported from `lib.rs` to produce a stable public API surface for integration tests and external consumers.
  - Features and optional deps are duplicated between `[dependencies]` and `[dev-dependencies]` for test-time optional features (see `Cargo.toml`). Keep versions in sync.
  - Use `NOCAPTURE=1` to surface logs from integration tests (separate crates) — otherwise `tracing` events are hidden.

- **Common quick commands:**
  - Build: `cargo build`
  - Build (nightly feature example): `cargo +nightly build --features log-slow-write-lock`
  - Run node locally: `cargo run` (or `cargo run --bin neptune-core`)
  - Run tests (show logs): `NOCAPTURE=1 cargo test -- --nocapture`
  - Run benches: `cargo bench`

- **When changing APIs or adding RPCs:**
  - Update `src/api` public helpers if the RPC should be usable programmatically (integration tests rely on it).
  - Add unit tests near implementation and an integration test under `tests/` demonstrating the public usage.

- **Where to look first when debugging:**
  - Initialization and wiring: [src/lib.rs](src/lib.rs#L1) (function `initialize`).
  - CLI and logging configuration: [src/main.rs](src/main.rs#L1).
  - RPC flow and server code: `src/application/json_rpc` and `src/application/rpc`.

If anything here is unclear or you want more detail (examples or extra file links), tell me which area to expand. I'll iterate quickly.
