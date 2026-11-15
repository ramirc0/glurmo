//! The controller's configuration, from `RESPONSE_BUILD_INFO`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::pack::Unpack;
use crate::slurm::{Site, rate_limit};

/// The request.
pub const REQUEST: u16 = 2001;
/// The reply.
pub const RESPONSE: u16 = 2002;

/// What glurmo needs from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Site facts.
    pub site: Site,
    /// `SlurmUser`'s UID, which alone may decode our credentials.
    pub slurm_uid: u32,
    /// slurmdbd's addresses, primary first, if the site keeps accounting there.
    pub accounting: Vec<String>,
    /// munged's socket, if `AuthInfo` names one.
    pub munge: Option<PathBuf>,
}

/// Decodes a `RESPONSE_BUILD_INFO` body.
///
/// # Errors
///
/// Fails when the body is shorter than Slurm 23.11's layout or a needed value is missing.
#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
pub fn decode(body: &[u8]) -> Result<Config> {
    let u = &mut Unpack::new(body);
    let _last_update = u.time()?;
    let _accounting_storage_enforce = u.u16()?;
    let accounting_backup = u.str()?;
    let accounting_host = u.str()?;
    let _accounting_storage_ext_host = u.str()?;
    let _accounting_storage_params = u.str()?;
    let accounting_port = u.u16()?;
    let _accounting_storage_tres = u.str()?;
    let accounting_type = u.str()?;
    let _accounting_storage_user = u.str()?;
    key_pairs(u)?;
    let _acct_gather_energy_type = u.str()?;
    let _acct_gather_filesystem_type = u.str()?;
    let _acct_gather_interconnect_type = u.str()?;
    let _acct_gather_node_freq = u.u16()?;
    let _acct_gather_profile_type = u.str()?;
    let _authalttypes = u.str()?;
    let _authalt_params = u.str()?;
    let auth_info = u.str()?;
    let _authtype = u.str()?;
    let _batch_start_timeout = u.u16()?;
    let _boot_time = u.time()?;
    let _bb_type = u.str()?;
    let _bcast_exclude = u.str()?;
    let _bcast_parameters = u.str()?;
    key_pairs(u)?;
    let _cli_filter_plugins = u.str()?;
    let cluster = u.str()?.context("no ClusterName")?;
    let _comm_params = u.str()?;
    let _complete_wait = u.u16()?;
    let _conf_flags = u.u32()?;
    let _control_addr = u.str_array()?;
    let _control_machine = u.str_array()?;
    let _core_spec_plugin = u.str()?;
    let _cpu_freq_def = u.u32()?;
    let _cpu_freq_govs = u.u32()?;
    let _cred_type = u.str()?;
    let _def_mem_per_cpu = u.u64()?;
    let _debug_flags = u.u64()?;
    let _dependency_params = u.str()?;
    let _eio_timeout = u.u16()?;
    let _enforce_part_limits = u.u16()?;
    let _epilog = u.str()?;
    let _epilog_msg_time = u.u32()?;
    let _epilog_slurmctld = u.str()?;
    key_pairs(u)?;
    let _ext_sensors_type = u.str()?;
    let _ext_sensors_freq = u.u16()?;
    let _fed_params = u.str()?;
    let _first_job_id = u.u32()?;
    let _fs_dampening_factor = u.u16()?;
    let _get_env_timeout = u.u16()?;
    let _gres_plugins = u.str()?;
    let _group_time = u.u16()?;
    let _group_force = u.u16()?;
    let _gpu_freq_def = u.str()?;
    let _hash_val = u.u32()?;
    let _health_check_interval = u.u16()?;
    let _health_check_node_state = u.u16()?;
    let _health_check_program = u.str()?;
    let _inactive_limit = u.u16()?;
    let _interactive_step_opts = u.str()?;
    let _job_acct_gather_freq = u.str()?;
    let _job_acct_gather_type = u.str()?;
    let _job_acct_gather_params = u.str()?;
    let _job_comp_host = u.str()?;
    let _job_comp_loc = u.str()?;
    let _job_comp_params = u.str()?;
    let _job_comp_port = u.u32()?;
    let _job_comp_type = u.str()?;
    let _job_comp_user = u.str()?;
    let _job_container_plugin = u.str()?;
    u.list(|u| {
        let _type = u.u16()?;
        u.u64().map(drop)
    })?;
    let _job_file_append = u.u16()?;
    let _job_requeue = u.u16()?;
    let _job_submit_plugins = u.str()?;
    let _kill_on_bad_exit = u.u16()?;
    let _kill_wait = u.u16()?;
    let _launch_params = u.str()?;
    let _licenses = u.str()?;
    let _log_fmt = u.u16()?;
    let _max_array_sz = u.u32()?;
    let _max_batch_requeue = u.u32()?;
    let _max_dbd_msgs = u.u32()?;
    let _mail_domain = u.str()?;
    let _mail_prog = u.str()?;
    let _max_job_cnt = u.u32()?;
    let _max_job_id = u.u32()?;
    let _max_mem_per_cpu = u.u64()?;
    let _max_node_cnt = u.u32()?;
    let _max_step_cnt = u.u32()?;
    let _max_tasks_per_node = u.u16()?;
    let _mcs_plugin = u.str()?;
    let _mcs_plugin_params = u.str()?;
    let min_job_age = u.u32()?;
    key_pairs(u)?;
    let _mpi_default = u.str()?;
    let _mpi_params = u.str()?;
    let _msg_timeout = u.u16()?;
    let _next_job_id = u.u32()?;
    u.list(|u| {
        let _name = u.str()?;
        key_pairs(u)
    })?;
    let _node_features_plugins = u.str()?;
    let _node_prefix = u.str()?;
    let _over_time_limit = u.u16()?;
    let _plugindir = u.str()?;
    let _plugstack = u.str()?;
    let _power_parameters = u.str()?;
    let _power_plugin = u.str()?;
    let _preempt_mode = u.u16()?;
    let _preempt_params = u.str()?;
    let _preempt_type = u.str()?;
    let _preempt_exempt_time = u.u32()?;
    let _prep_params = u.str()?;
    let _prep_plugins = u.str()?;
    let _priority_decay_hl = u.u32()?;
    let _priority_calc_period = u.u32()?;
    let _priority_favor_small = u.u16()?;
    let _priority_flags = u.u16()?;
    let _priority_max_age = u.u32()?;
    let _priority_params = u.str()?;
    let _priority_reset_period = u.u16()?;
    let _priority_type = u.str()?;
    let _priority_weights = [u.u32()?, u.u32()?, u.u32()?, u.u32()?, u.u32()?, u.u32()?];
    let _priority_weight_tres = u.str()?;
    let private_data = u.u16()?;
    let _proctrack_type = u.str()?;
    let _prolog = u.str()?;
    let _prolog_epilog_timeout = u.u16()?;
    let _prolog_slurmctld = u.str()?;
    let _prolog_flags = u.u16()?;
    let _propagate_prio_process = u.u16()?;
    let _propagate_rlimits = u.str()?;
    let _propagate_rlimits_except = u.str()?;
    let _reboot_program = u.str()?;
    let _reconfig_flags = u.u16()?;
    let _requeue_exit = u.str()?;
    let _requeue_exit_hold = u.str()?;
    let _resume_fail_program = u.str()?;
    let _resume_program = u.str()?;
    let _resume_rate = u.u16()?;
    let _resume_timeout = u.u16()?;
    let _resv_epilog = u.str()?;
    let _resv_over_run = u.u16()?;
    let _resv_prolog = u.str()?;
    let _ret2service = u.u16()?;
    let _sched_params = u.str()?;
    let _sched_logfile = u.str()?;
    let _sched_log_level = u.u16()?;
    let _sched_time_slice = u.u16()?;
    let _schedtype = u.str()?;
    let _scron_params = u.str()?;
    let _select_type = u.str()?;
    key_pairs(u)?;
    let _select_type_param = u.u16()?;
    let _slurm_conf = u.str()?;
    let slurm_uid = u.u32()?;
    let _slurm_user_name = u.str()?;
    let _slurmd_user_id = u.u32()?;
    let _slurmd_user_name = u.str()?;
    let _slurmctld_addr = u.str()?;
    let _slurmctld_debug = u.u16()?;
    let _slurmctld_logfile = u.str()?;
    let slurmctld_params = u.str()?;
    let _slurmctld_pidfile = u.str()?;
    let _slurmctld_port = u.u32()?;
    let _slurmctld_port_count = u.u16()?;
    let _slurmctld_primary_off_prog = u.str()?;
    let _slurmctld_primary_on_prog = u.str()?;
    let _slurmctld_syslog_debug = u.u16()?;
    let _slurmctld_timeout = u.u16()?;
    let _slurmd_debug = u.u16()?;
    let _slurmd_logfile = u.str()?;
    let _slurmd_params = u.str()?;
    let _slurmd_pidfile = u.str()?;
    let _slurmd_port = u.u32()?;
    let _slurmd_spooldir = u.str()?;
    let _slurmd_syslog_debug = u.u16()?;
    let _slurmd_timeout = u.u16()?;
    let _srun_epilog = u.str()?;
    let _srun_port_range = [u.u16()?, u.u16()?];
    let _srun_prolog = u.str()?;
    let _state_save_location = u.str()?;
    let _suspend_exc_nodes = u.str()?;
    let _suspend_exc_parts = u.str()?;
    let _suspend_exc_states = u.str()?;
    let _suspend_program = u.str()?;
    let _suspend_rate = u.u16()?;
    let _suspend_time = u.u32()?;
    let _suspend_timeout = u.u16()?;
    let _switch_param = u.str()?;
    let _switch_type = u.str()?;
    let _task_epilog = u.str()?;
    let _task_prolog = u.str()?;
    let _task_plugin = u.str()?;
    let _task_plugin_param = u.u32()?;
    let _tcp_timeout = u.u16()?;
    let _tmp_fs = u.str()?;
    let _topology_param = u.str()?;
    let _topology_plugin = u.str()?;
    let _tree_width = u.u16()?;
    let _unkillable_program = u.str()?;
    let _unkillable_timeout = u.u16()?;
    let version = u.str()?.context("no SLURM_VERSION")?;

    let accounting = if accounting_type.as_deref() == Some("accounting_storage/slurmdbd") {
        [accounting_host, accounting_backup]
            .into_iter()
            .flatten()
            .map(|host| format!("{host}:{accounting_port}"))
            .collect()
    } else {
        Vec::new()
    };
    Ok(Config {
        site: Site {
            cluster,
            version: version.parse()?,
            min_job_age_s: u64::from(min_job_age),
            private_data: private_data_names(private_data),
            rate_limit: rate_limit(slurmctld_params.as_deref().unwrap_or_default())?,
        },
        slurm_uid,
        accounting,
        munge: auth_info.as_deref().and_then(munge_socket),
    })
}

fn key_pairs(u: &mut Unpack) -> Result<()> {
    u.list(|u| u.skip_strs(2)).map(drop)
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
}
