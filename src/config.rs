//! `config.ron`: how often to sample, where history lives, where the server listens, and how to name people.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ron::extensions::Extensions;
use serde::{Deserialize, Serialize};

use crate::slurm::{Backend, Intervals};
use glurmo_model::Login;

/// Everything `config.ron` can set. Every field has a default.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How to reach Slurm.
    pub slurm: Reach,
    /// How often to ask Slurm for each kind of data.
    pub sample: Intervals,
    /// Where history lives and how long it stays.
    pub store: Storage,
    /// The server.
    pub server: Server,
    /// How to show people.
    pub names: Names,
    /// The terminal view.
    pub tui: Tui,
    /// GPU models by partition, for nodes whose GRES names no type.
    pub gpu_models: BTreeMap<String, String>,
}

/// How to reach Slurm.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reach {
    /// The protocol, or Slurm's commands as a fallback.
    pub backend: Backend,
    /// slurmctld's `host:port` addresses, primary first. Empty reads them from slurm.conf.
    pub controller: Vec<String>,
}

/// Where history lives and how long it stays.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    /// The SQLite file, which must be on a local disk. `None` puts it under `$XDG_STATE_HOME/glurmo`.
    pub path: Option<PathBuf>,
    /// How long to keep history.
    pub keep: Keep,
    /// How much accounting to read into a new store.
    pub backfill: Days,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            path: None,
            keep: Keep::Forever,
            backfill: Days(30),
        }
    }
}

/// How long to keep history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Keep {
    /// Keep everything.
    Forever,
    /// Keep this many days.
    Days(u32),
}

/// A number of days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Days(pub u32);

/// The server.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    /// The address to listen on.
    pub listen: SocketAddr,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8765)),
        }
    }
}

/// The terminal view.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tui {
    /// The server to follow, such as `http://head:8765`. `None` samples the cluster in-process.
    pub server: Option<String>,
    /// The layout to start with.
    pub layout: String,
    /// The theme to start with.
    pub theme: String,
    /// Whose jobs `m` and the `Mine` panel show. `None` takes `$USER`.
    pub me: Option<Login>,
}

impl Default for Tui {
    fn default() -> Self {
        Self {
            server: None,
            layout: "tui".to_owned(),
            theme: "rose-pine".to_owned(),
            me: None,
        }
    }
}

/// How to show people.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum Names {
    /// By login.
    #[default]
    Login,
    /// By the full name in the account database, or the login when it has none.
    FullName,
}

impl Config {
    /// Reads the config at `path`, or at `$XDG_CONFIG_HOME/glurmo/config.ron` when `path` is `None`.
    /// A missing default file gives the defaults.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or does not parse, or `path` names a missing file.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = if let Some(path) = path {
            path.to_owned()
        } else {
            let path = crate::xdg::config_home()?.join("glurmo/config.ron");
            if !path.exists() {
                return Ok(Self::default());
            }
            path
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        from_ron(&text).with_context(|| format!("{} does not parse", path.display()))
    }
}

/// Parses RON with `implicit_some` on, so files can skip `Some(...)`.
///
/// # Errors
///
/// Fails when `text` does not parse as `T`.
pub fn from_ron<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    Ok(ron::Options::default()
        .with_default_extension(Extensions::IMPLICIT_SOME)
        .from_str(text)?)
}

/// Writes `value` as pretty RON with struct names, the way the config files look.
///
/// # Errors
///
/// Fails when `value` cannot be written as RON.
pub fn to_ron<T: Serialize>(value: &T) -> Result<String> {
    let pretty = ron::ser::PrettyConfig::new().struct_names(true);
    Ok(ron::ser::to_string_pretty(value, pretty)?)
}

/// A key glurmo no longer reads. Any value parses, so an older config still loads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retired;

impl<'de> Deserialize<'de> for Retired {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

/// Reads and writes whole seconds as a duration such as `"10s"` or `"10m"`.
pub mod seconds {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    /// Writes `10` as `"10s"`.
    ///
    /// # Errors
    ///
    /// Fails when the serializer does.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` passes a reference"
    )]
    pub fn serialize<S: Serializer>(seconds: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{seconds}s"))
    }

    /// Reads a duration such as `"10s"`, `"10m"` or `"1h 30m"` as whole seconds.
    ///
    /// # Errors
    ///
    /// Fails on text that is not a duration, or on one shorter than a second.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        let duration: jiff::SignedDuration = text.parse().map_err(D::Error::custom)?;
        u64::try_from(duration.as_secs())
            .ok()
            .filter(|&seconds| seconds >= 1)
            .ok_or_else(|| D::Error::custom(format!("{text:?} is shorter than a second")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_config_parses() {
        let config: Config = from_ron(
            r#"Config(
                sample: (jobs: "15s", nodes: "1m", partitions: "10m", lookups: 10),
                store: (path: "/scratch/otter/glurmo/zlurm.sqlite", keep: Days(365), backfill: Days(7)),
                server: (listen: "0.0.0.0:8765"),
                names: FullName,
                tui: (server: "http://head:8765", layout: "wide", theme: "rose-pine-dawn", me: "otter"),
                gpu_models: {"gpu": "V100", "gpuh200": "H200"},
            )"#,
        )
        .unwrap();
        assert_eq!(
            config.sample,
            Intervals {
                jobs_s: 15,
                nodes_s: 60,
                partitions_s: 600,
                lookups: 10,
                ..Intervals::default()
            }
        );
        assert_eq!(
            config.store,
            Storage {
                path: Some("/scratch/otter/glurmo/zlurm.sqlite".into()),
                keep: Keep::Days(365),
                backfill: Days(7),
            }
        );
        assert_eq!(config.server.listen.to_string(), "0.0.0.0:8765");
        assert_eq!(config.names, Names::FullName);
        assert_eq!(config.tui.server.as_deref(), Some("http://head:8765"));
        assert_eq!(config.tui.layout, "wide");
        assert_eq!(config.tui.me, Some(Login("otter".into())));
        assert_eq!(config.gpu_models["gpuh200"], "H200");
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let config: Config = from_ron(r#"(sample: (jobs: "20s"))"#).unwrap();
        assert_eq!(config.sample.jobs_s, 20);
        assert_eq!(config.sample.nodes_s, 30);
        assert_eq!(config.store, Storage::default());
        assert_eq!(config.tui, Tui::default());
    }

    #[test]
    fn typos_and_bad_durations_are_errors() {
        assert!(from_ron::<Config>("(nmaes: Login)").is_err());
        assert!(from_ron::<Config>(r#"(sample: (jobs: "soon"))"#).is_err());
        assert!(from_ron::<Config>(r#"(sample: (jobs: "-5s"))"#).is_err());
        assert!(from_ron::<Config>(r#"(sample: (jobs: "0s"))"#).is_err());
        assert!(from_ron::<Config>(r#"(sample: (nodes: "500ms"))"#).is_err());
    }

    #[test]
    fn a_stats_interval_from_an_older_config_is_ignored() {
        let config = from_ron::<Config>(r#"(sample: (jobs: "20s", stats: "60s"))"#).unwrap();
        assert_eq!(config.sample.jobs_s, 20);
        assert_eq!(
            config.sample,
            from_ron::<Config>(r#"(sample: (jobs: "20s"))"#)
                .unwrap()
                .sample
        );
    }
}
