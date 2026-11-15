//! Replays of recorded zlurm traffic for tests.

use std::io::Write;

use crate::config::{Config, from_ron};
use crate::hub::Hub;
use crate::sampler::Sampler;
use crate::slurm::Slurm;
use crate::slurm::source::{Exchange, Replay, Source};

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
    let mut recording = tempfile::NamedTempFile::new().unwrap();
    for line in std::io::BufRead::lines(std::io::BufReader::new(decoder)) {
        let mut exchange: Exchange = serde_json::from_str(&line.unwrap()).unwrap();
        edit(&mut exchange);
        writeln!(recording, "{}", serde_json::to_string(&exchange).unwrap()).unwrap();
    }
    Slurm::connect(Source::Replay(Replay::open(recording.path()).unwrap()))
        .await
        .unwrap()
}

/// A hub that has published the whole busy quarter hour, sampled without a store.
pub async fn busy_hub() -> Hub {
    let slurm = busy_quarter_hour().await;
    let hub = Hub::new(slurm.site().clone());
    let mut sampler = Sampler::new(slurm, &zlurm_config(), None, hub.clone());
    sampler.run(None).await;
    hub
}
