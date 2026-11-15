# List the recipes.
default:
    @just --list

# Check formatting, lint with clippy, build the docs and run the tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
    cargo test

# Run rtviz with arguments.
run *args:
    cargo run -- {{args}}

# Build a static musl binary at target/x86_64-unknown-linux-musl/release/rtviz. It runs on any x86-64 Linux.
static:
    cargo build --release --target x86_64-unknown-linux-musl

# Build a dynamic glibc binary at target/release/rtviz. It runs only where glibc is at least as new as here.
release:
    cargo build --release
