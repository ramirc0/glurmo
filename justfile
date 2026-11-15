# List the recipes.
default:
    @just --list

# Check formatting, lint with clippy, build the docs and run the tests.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p rtviz-web --target wasm32-unknown-unknown -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    cargo test --workspace

# Run rtviz with arguments.
run *args:
    cargo run -- {{args}}

# Build the web view into web/dist, which release builds of rtviz embed.
web:
    cd web && trunk build --release

# Load the web view from a running server in headless Chromium, print its console and save a screenshot.
web-check server out *args:
    node web/check.mjs {{server}} {{out}} {{args}}

# Build a static musl binary at target/x86_64-unknown-linux-musl/release/rtviz. It runs on any x86-64 Linux.
static: web
    cargo build --release --target x86_64-unknown-linux-musl

# Build a dynamic glibc binary at target/release/rtviz. It runs only where glibc is at least as new as here.
release: web
    cargo build --release
