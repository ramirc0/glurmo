//! Rebuilds glurmo when the web view changes, because release builds embed `crates/web/dist` and cargo
//! otherwise misses files that trunk adds after the first build.

fn main() {
    println!("cargo:rerun-if-changed=crates/web/dist");
}
