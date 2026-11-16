# List the recipes.
default:
    @just --list

# Check formatting, lint with clippy, build the docs and run the tests.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p glurmo-web --target wasm32-unknown-unknown -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
    cargo test --workspace

# Run glurmo with arguments.
run *args:
    cargo run -- {{args}}

# Build the web view into web/dist, which release builds of glurmo embed.
web:
    cd web && trunk build --release

# Load the web view from a running server in headless Chromium, print its console and save a screenshot.
web-check server out *args:
    node web/check.mjs {{server}} {{out}} {{args}}

# Replay the busy fixture with every job in its own row and its first queue held twice over, about
# 1,400 rows, and fail if hovering a row slows down after 200 rebuilds of the jobs table.
hover-check port="8793": web
    #!/usr/bin/env bash
    set -euo pipefail
    server=
    dir=$(mktemp -d -p "${SCRATCH:-${TMPDIR:-/tmp}}")
    trap '[ -n "$server" ] && kill "$server"; rm -rf "$dir"' EXIT
    zstd -dc tests/fixtures/slurm-23.11/busy-15m.jsonl.zst | jq -c -s '
        def own: .name = "j\(.key.job)" | .array = null;
        def jobs: .call == "Jobs" and .answer.Ok;
        (first(.[] | select(jobs)).answer.Ok.Jobs | map(select(.state == "pending"))) as $queue
        | [range(1; 3) as $copy | $queue[] | .key.job += $copy * 10000000 | own] as $more
        | .[] | if jobs then .answer.Ok.Jobs |= map(own) + $more else . end
        ' > "$dir/replay.jsonl"
    printf '(server: (listen: "127.0.0.1:{{port}}"))\n' > "$dir/config.ron"
    cargo build -q
    XDG_CONFIG_HOME="$dir" target/debug/glurmo --config "$dir/config.ron" \
        serve --replay "$dir/replay.jsonl" --speed 1000 2> "$dir/serve.log" &
    server=$!
    until grep -q "replay ran out" "$dir/serve.log"; do kill -0 "$server"; sleep 0.2; done
    SCRATCH="$dir" node web/check.mjs http://127.0.0.1:{{port}} "$dir/jobs.png" \
        --query '?layout=jobs' --eval "$(cat web/hover-check.js)"

# Run the tests with the store on TimescaleDB, and the ignored store tests that need a server: start
# TimescaleDB 2.31.0 on PostgreSQL 18 from its image under Apptainer, on a Unix socket in a scratch
# directory, and stop it afterwards. Its database sorts text by English rules, as many servers do.
timescale-check:
    #!/usr/bin/env bash
    set -euo pipefail
    image=docker://timescale/timescaledb@sha256:0fcff5761363809d1242f2261a3482197b5a5337407f8b97a976fb174206d579
    scratch="${SCRATCH:-${TMPDIR:-/tmp}}"
    export APPTAINER_CACHEDIR="${APPTAINER_CACHEDIR:-$scratch/apptainer}"
    mkdir -p "$APPTAINER_CACHEDIR"
    dir=$(mktemp -d -p "$scratch")
    in_image() { apptainer exec --bind "$dir" "$image" "$@"; }
    trap 'in_image pg_ctl -D "$dir/data" -m fast -w stop > /dev/null || true; rm -rf "$dir"' EXIT
    in_image initdb -D "$dir/data" --auth=trust > "$dir/initdb.log"
    in_image postgres -D "$dir/data" -k "$dir" -c listen_addresses= \
        -c shared_preload_libraries=timescaledb -c timescaledb.telemetry_level=off \
        > "$dir/server.log" 2>&1 &
    server=$!
    until in_image pg_isready -q -h "$dir"; do kill -0 "$server"; sleep 0.2; done
    in_image psql -q -h "$dir" -d postgres -c "CREATE DATABASE glurmo TEMPLATE template0
        LOCALE_PROVIDER icu ICU_LOCALE 'en-US' LOCALE 'C.UTF-8'"
    in_image psql -q -h "$dir" -d glurmo -c 'CREATE EXTENSION timescaledb'
    export GLURMO_TEST_TIMESCALE="host=$dir dbname=glurmo"
    cargo test
    cargo test -- --ignored store::

# Build a static musl binary at target/x86_64-unknown-linux-musl/release/glurmo. It runs on any x86-64 Linux.
static: web
    cargo build --release --target x86_64-unknown-linux-musl

# Build a dynamic glibc binary at target/release/glurmo. It runs only where glibc is at least as new as here.
release: web
    cargo build --release
