//! `rtviz` shows who holds which cores and GPUs on a batch-scheduler cluster.

use clap::Parser;

/// Live view of who holds which cores and GPUs on a cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
