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

# Build a static binary at target/x86_64-unknown-linux-musl/release/rtviz.
static:
    cargo build --release --target x86_64-unknown-linux-musl
