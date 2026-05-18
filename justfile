default:
    @just --list

# --- Frontend ---

# Build the wasm bundle (release). Required before any cargo build
# of the daemon — build.rs embeds frontend/dist/ via include_dir!.
build-frontend:
    cd frontend && trunk build --release

# Debug wasm bundle. Faster; used by build-dev / test / serve-dev.
build-frontend-dev:
    cd frontend && trunk build

# Watch frontend; auto-rebuild dist/ on change. Pair with serve-dev.
frontend-watch:
    cd frontend && trunk watch

# --- Daemon ---

# Release daemon with the freshly-built frontend embedded.
build: build-frontend
    cargo build --release -p trinity

# Debug build of everything.
build-dev: build-frontend-dev
    cargo build --workspace

# --- Test / CI ---

test: build-frontend-dev
    cargo test --workspace --exclude trinity-frontend

# Everything CI checks (fmt, clippy, tests, wasm-clean, frontend).
check: build-frontend
    cargo fmt -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo check -p trinity-core --target wasm32-unknown-unknown
    cargo test --workspace --exclude trinity-frontend

# --- Run ---

serve: build
    ./target/release/trinity serve

# Kill the running daemon, rebuild, restart in background.
restart: build
    -pkill -f "target/release/trinity serve"
    sleep 1
    nohup ./target/release/trinity serve > /tmp/trinity-serve.log 2>&1 &
    sleep 2
    curl -sf http://127.0.0.1:7777/healthz && echo " — trinity running"

# Frontend dev loop: daemon serves frontend/dist/ from disk, so
# `just frontend-watch` in another terminal flows through without
# rebuilding the daemon.
serve-dev: build-frontend-dev
    cargo run --release -p trinity -- \
        serve --frontend-dist frontend/dist

# --- Cleanup ---

clean:
    cargo clean
    rm -rf frontend/dist
