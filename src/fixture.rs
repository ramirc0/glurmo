//! Replays of recorded zlurm traffic for tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::{Config, from_ron};
use crate::hub::Hub;
use crate::sampler::Sampler;
use crate::slurm::recording::{Exchange, Replay};
use crate::slurm::{Slurm, Source};
use crate::store::connection::Connection;
use crate::store::timescale::{self, Timescale};
use crate::store::{History, Settings, Store};

/// The variable that names a TimescaleDB server for tests, as a key-value connection string.
const TIMESCALE: &str = "GLURMO_TEST_TIMESCALE";

/// How many schemas this process has made, so each gets a name of its own.
static SCHEMAS: AtomicU32 = AtomicU32::new(0);

/// Where a test keeps its store: a SQLite file in a temporary directory, or a schema of its own on a
/// TimescaleDB server. The directory and the schema are deleted when this drops.
pub struct StoreHome {
    dir: tempfile::TempDir,
    /// The server and the schema on it, for a store on TimescaleDB.
    schema: Option<(String, String)>,
}

impl StoreHome {
    /// A home on the TimescaleDB server that `GLURMO_TEST_TIMESCALE` names, or in SQLite without it.
    pub fn new() -> Self {
        if std::env::var_os(TIMESCALE).is_some() {
            Self::timescale()
        } else {
            Self::sqlite()
        }
    }

    /// A home in SQLite.
    pub fn sqlite() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            schema: None,
        }
    }

    /// A home on the TimescaleDB server that `GLURMO_TEST_TIMESCALE` names.
    pub fn timescale() -> Self {
        let server = std::env::var(TIMESCALE).expect("GLURMO_TEST_TIMESCALE names a server");
        let schema = format!(
            "glurmo_test_{}_{}",
            std::process::id(),
            SCHEMAS.fetch_add(1, Ordering::Relaxed)
        );
        Timescale::connect(&timescale::parse(&server).unwrap())
            .unwrap()
            .execute_batch(&format!("CREATE SCHEMA {schema}"))
            .unwrap();
        let mut home = Self::sqlite();
        home.schema = Some((server, schema));
        home
    }

    /// Opens the store, which is created at its first opening.
    pub fn open(&self, backfill_days: u32) -> Store {
        let settings = self.settings(backfill_days);
        match self.server() {
            Some(server) => Store::connect(&server, settings),
            None => Store::open(&self.path(), settings),
        }
        .unwrap()
    }

    /// Settings that keep everything and put a SQLite store's backup in this home's directory.
    pub fn settings(&self, backfill_days: u32) -> Settings {
        Settings {
            backfill_days,
            keep_days: None,
            backup: self.dir.path().join("backup/glurmo.sqlite"),
        }
    }

    /// A connection that reads what the store holds.
    pub fn read(&self) -> Connection {
        match self.server() {
            Some(server) => Connection::Timescale(
                Timescale::connect(&timescale::parse(&server).unwrap()).unwrap(),
            ),
            None => Connection::Sqlite(rusqlite::Connection::open(self.path()).unwrap()),
        }
    }

    /// This home's directory.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Where a SQLite store's file is.
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("glurmo.sqlite")
    }

    /// The connection string that reaches this home's schema, for a store on TimescaleDB.
    pub fn server(&self) -> Option<String> {
        self.schema.as_ref().map(|(server, schema)| {
            format!("{server} application_name={schema} options='-c search_path={schema},public'")
        })
    }
}

impl Drop for StoreHome {
    fn drop(&mut self) {
        if let Some((server, schema)) = &self.schema
            && let Ok(config) = timescale::parse(server)
            && let Ok(db) = Timescale::connect(&config)
        {
            let _ = db.execute_batch(&format!("DROP SCHEMA {schema} CASCADE"));
        }
    }
}

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

/// The busy quarter hour sampled into a store, kept until its home drops, the hub it published to,
/// and the store's history.
pub async fn busy_store() -> (StoreHome, Hub, History) {
    busy_store_in(StoreHome::new()).await
}

/// The busy quarter hour sampled into a store in `home`, as [`busy_store`] gives it.
pub async fn busy_store_in(home: StoreHome) -> (StoreHome, Hub, History) {
    let store = home.open(0);
    let history = store.history();
    let hub = sample_busy_quarter_hour(Some(store)).await;
    (home, hub, history)
}

async fn sample_busy_quarter_hour(store: Option<Store>) -> Hub {
    let slurm = busy_quarter_hour().await;
    let hub = Hub::new(slurm.site().clone());
    let mut sampler = Sampler::new(slurm, &zlurm_config(), store, hub.clone());
    sampler.run(None).await;
    hub
}
