default:
    @just --list

# Build the release daemon. build.rs runs `trunk build --release`
# in frontend/ and bakes the resulting dist/ into the binary, so
# daemon and SPA versions cannot drift.
build:
    cargo build --release -p trinity

# Build everything for development (debug).
build-dev:
    TRINITY_SKIP_FRONTEND_BUILD=1 cargo build --workspace
    cd frontend && trunk build

# Full test suite.
test:
    TRINITY_SKIP_FRONTEND_BUILD=1 cargo test --workspace --exclude trinity-frontend

# Everything CI checks (fmt, clippy, tests, wasm-clean, trunk).
# Mirrors the verification commands in the plan files.
check:
    cargo fmt -- --check
    TRINITY_SKIP_FRONTEND_BUILD=1 cargo clippy --workspace --all-targets -- -D warnings
    cargo check -p trinity-core --target wasm32-unknown-unknown
    TRINITY_SKIP_FRONTEND_BUILD=1 cargo test --workspace --exclude trinity-frontend
    cd frontend && trunk build

# Run the release daemon. Rebuilds first; the build.rs guarantee
# means the embedded SPA matches this binary.
serve: build
    ./target/release/trinity serve

# Restart the running daemon: kill the old one, rebuild, start
# fresh in the background. Used after editing daemon or frontend
# source so the next page load gets the fresh wasm.
restart: build
    -pkill -f "target/release/trinity serve"
    sleep 1
    nohup ./target/release/trinity serve > /tmp/trinity-serve.log 2>&1 &
    sleep 2
    curl -sf http://127.0.0.1:7777/healthz && echo " — trinity running"

# Frontend dev loop: keep daemon running with --frontend-dist set
# to frontend/dist, then run `trunk watch` in another terminal.
# Frontend changes flow through without rebuilding the daemon.
serve-dev:
    cd frontend && trunk build
    TRINITY_SKIP_FRONTEND_BUILD=1 cargo run --release -p trinity -- \
        serve --frontend-dist frontend/dist

# Watch frontend; auto-rebuild dist/ on change. Pair with
# `just serve-dev` in another terminal.
frontend-watch:
    cd frontend && trunk watch

# Wipe build artifacts and the embedded frontend dist.
clean:
    cargo clean
    rm -rf frontend/dist
