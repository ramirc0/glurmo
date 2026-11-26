//! The controller's configuration, reduced to what glurmo reads.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// What glurmo needs from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `ClusterName`.
    pub cluster: String,
    /// The controller's Slurm version, such as `23.11.4`.
    pub version: String,
    /// `MinJobAge`: how long ended jobs stay in the controller's job list, in seconds.
    pub min_job_age_s: u64,
    /// What `PrivateData` hides from other users, named as scontrol names it, such as `jobs`.
    pub private_data: Vec<String>,
    /// `SlurmctldParameters`, which hold the RPC rate limit.
    pub slurmctld_parameters: Option<String>,
    /// slurmdbd's addresses, primary first, if the site keeps accounting there.
    pub accounting: Vec<String>,
    /// munged's socket, if `AuthInfo` names one.
    pub munge: Option<PathBuf>,
}

/// What glurmo needs from the configuration.
///
/// # Errors
///
/// Fails when the configuration lacks the cluster's name or the Slurm version.
pub fn config(configuration: slurm_client::Configuration) -> Result<Config> {
    Raw {
        accounting_backup: configuration.accounting_storage_backup_host,
        accounting_host: configuration.accounting_storage_host,
        accounting_port: configuration.accounting_storage_port,
        accounting_type: configuration.accounting_storage_type,
        auth_info: configuration.auth_info,
        cluster: configuration.cluster_name,
        min_job_age: configuration.min_job_age.as_secs(),
        private_data: configuration.private_data,
        slurmctld_params: configuration.slurmctld_parameters,
        version: configuration.version,
    }
    .config()
}

/// The settings glurmo reads, as the controller packed them.
struct Raw {
    accounting_backup: Option<String>,
    accounting_host: Option<String>,
    accounting_port: u16,
    accounting_type: Option<String>,
    auth_info: Option<String>,
    cluster: Option<String>,
    min_job_age: u64,
    private_data: u16,
    slurmctld_params: Option<String>,
    version: Option<String>,
}

impl Raw {
    fn config(self) -> Result<Config> {
        let accounting = if self.accounting_type.as_deref() == Some("accounting_storage/slurmdbd") {
            [self.accounting_host, self.accounting_backup]
                .into_iter()
                .flatten()
                .map(|host| format!("{host}:{}", self.accounting_port))
                .collect()
        } else {
            Vec::new()
        };
        Ok(Config {
            cluster: self.cluster.context("no ClusterName")?,
            version: self.version.context("no SLURM_VERSION")?,
            min_job_age_s: self.min_job_age,
            private_data: private_data_names(self.private_data),
            slurmctld_parameters: self.slurmctld_params,
            accounting,
            munge: self.auth_info.as_deref().and_then(munge_socket),
        })
    }
}

/// Names the `PrivateData` bits as scontrol does.
fn private_data_names(bits: u16) -> Vec<String> {
    [
        (1 << 5, "accounts"),
        (1 << 8, "events"),
        (1 << 0, "jobs"),
        (1 << 1, "nodes"),
        (1 << 2, "partitions"),
        (1 << 6, "reservations"),
        (1 << 3, "usage"),
        (1 << 4, "users"),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .map(|(_, name)| name.to_owned())
    .collect()
}

/// Reads `socket=PATH` from `AuthInfo`, as `slurm_auth_opts_to_socket` does.
fn munge_socket(auth_info: &str) -> Option<PathBuf> {
    auth_info
        .split(',')
        .find_map(|option| option.strip_prefix("socket="))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_data_bits_name_in_scontrol_order() {
        assert_eq!(private_data_names(0), Vec::<String>::new());
        assert_eq!(
            private_data_names(0b1_0000_0011),
            ["events", "jobs", "nodes"]
        );
    }

    #[test]
    fn auth_info_may_name_the_socket() {
        assert_eq!(
            munge_socket("cred_expire=30,socket=/var/run/m.sock"),
            Some(PathBuf::from("/var/run/m.sock"))
        );
        assert_eq!(munge_socket("cred_expire=30"), None);
    }

    fn raw() -> Raw {
        Raw {
            accounting_backup: Some("dbd-2".into()),
            accounting_host: Some("dbd-1".into()),
            accounting_port: 6819,
            accounting_type: Some("accounting_storage/slurmdbd".into()),
            auth_info: Some("socket=/run/m.sock".into()),
            cluster: Some("zlurm".into()),
            min_job_age: 300,
            private_data: 1,
            slurmctld_params: Some("rl_enable,rl_bucket_size=50".into()),
            version: Some("23.11.4".into()),
        }
    }

    #[test]
    fn every_setting_lands_in_its_own_field() {
        let expected = Config {
            cluster: "zlurm".into(),
            version: "23.11.4".into(),
            min_job_age_s: 300,
            private_data: vec!["jobs".into()],
            slurmctld_parameters: Some("rl_enable,rl_bucket_size=50".into()),
            accounting: vec!["dbd-1:6819".into(), "dbd-2:6819".into()],
            munge: Some(PathBuf::from("/run/m.sock")),
        };
        assert_eq!(raw().config().unwrap(), expected);
    }

    #[test]
    fn a_site_without_slurmdbd_has_no_accounting_addresses() {
        let local = Raw {
            accounting_type: Some("accounting_storage/none".into()),
            ..raw()
        };
        assert_eq!(local.config().unwrap().accounting, Vec::<String>::new());
    }
}
