//! Replays of recorded zlurm traffic for tests.

use crate::config::{Config, from_ron};
use crate::hub::Hub;
use crate::sampler::Sampler;
use crate::slurm::recording::{Exchange, Replay};
use crate::slurm::{Slurm, Source};
use crate::store::{Settings, Store};

/// The defaults, with zlurm's GPU models.
pub fn zlurm_config() -> Config {
    from_ron(r#"(gpu_models: {"gpu": "V100", "gpuh200": "H200"})"#).unwrap()
}

/// Connects to a replay of a pseudonymized busy quarter hour on zlurm.
pub async fn busy_quarter_hour() -> Slurm {
    busy_quarter_hour_with(|_| {}).await
}

/// Connects to the busy quarter hour, with each recorded exchange passed through `edit` first.
pub async fn busy_quarter_hour_with(mut edit: impl FnMut(&mut Exchange)) -> Slurm {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/slurm-23.11/busy-15m.jsonl.zst");
    let mut compressed = std::fs::File::open(path).unwrap();
    let decoder = ruzstd::decoding::StreamingDecoder::new(&mut compressed).unwrap();
    let exchanges = std::io::BufRead::lines(std::io::BufReader::new(decoder))
        .map(|line| {
            let mut exchange: Exchange = serde_json::from_str(&line.unwrap()).unwrap();
            edit(&mut exchange);
            exchange
        })
        .collect();
    Slurm::connect(Source::Replay(Replay::new(exchanges)), None)
        .await
        .unwrap()
}

/// A hub that has published the whole busy quarter hour, sampled without a store.
pub async fn busy_hub() -> Hub {
    sample_busy_quarter_hour(None).await
}

/// A hub that has published the whole busy quarter hour, sampled into a temporary store as a server
/// would. Unlike [`busy_hub`], it names users and gives them colors.
pub async fn busy_stored_hub() -> Hub {
    busy_store().await.1
}

/// The busy quarter hour sampled into a store, kept until the directory drops, and the hub it published
/// to.
pub async fn busy_store() -> (tempfile::TempDir, Hub) {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        backfill_days: 0,
        keep_days: None,
        backup: dir.path().join("backup.sqlite"),
    };
    let store = Store::open(&store_path(&dir), settings).unwrap();
    let hub = sample_busy_quarter_hour(Some(store)).await;
    (dir, hub)
}

/// Where [`busy_store`] keeps its store.
pub fn store_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("glurmo.sqlite")
}

async fn sample_busy_quarter_hour(store: Option<Store>) -> Hub {
    let slurm = busy_quarter_hour().await;
    let hub = Hub::new(slurm.site().clone());
    let mut sampler = Sampler::new(slurm, &zlurm_config(), store, hub.clone());
    sampler.run(None).await;
    hub
}
