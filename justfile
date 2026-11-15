# List the recipes.
default:
    @just --list

# Check formatting, lint with clippy, build the docs and run the tests.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    cargo test --workspace

# Run rtviz with arguments.
run *args:
    cargo run -- {{args}}

# Build a static musl binary at target/x86_64-unknown-linux-musl/release/rtviz. It runs on any x86-64 Linux.
static:
    cargo build --release --target x86_64-unknown-linux-musl

# Build a dynamic glibc binary at target/release/rtviz. It runs only where glibc is at least as new as here.
release:
    cargo build --release
