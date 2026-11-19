//! The controller's configuration, from `RESPONSE_BUILD_INFO`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::Protocol;
use super::pack::Unpack;

/// The request.
pub(crate) const REQUEST: u16 = 2001;
/// The reply.
pub(crate) const RESPONSE: u16 = 2002;

/// What glurmo needs from the configuration.
///
/// # Examples
///
/// ```
/// use glurmo_rpc::config::Config;
///
/// let config = Config {
///     cluster: "zlurm".into(),
///     version: "23.11.4".into(),
///     min_job_age_s: 300,
///     private_data: vec!["jobs".into()],
///     slurmctld_parameters: Some("rl_enable,rl_bucket_size=50".into()),
///     slurm_uid: 450,
///     accounting: vec!["dbd-1:6819".into()],
///     munge: None,
/// };
/// assert!(config.private_data.iter().any(|hidden| hidden == "jobs"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `ClusterName`.
    pub cluster: String,
    /// The controller's Slurm version, such as `23.11.4`.
    pub version: String,
    /// `MinJobAge`: how long ended jobs stay in the controller's job list, in seconds.
    pub min_job_age_s: u32,
    /// What `PrivateData` hides from other users, named as scontrol names it, such as `jobs`.
    pub private_data: Vec<String>,
    /// `SlurmctldParameters`, which hold the RPC rate limit.
    pub slurmctld_parameters: Option<String>,
    /// `SlurmUser`'s UID, which alone may decode our credentials.
    pub slurm_uid: u32,
    /// slurmdbd's addresses, primary first, if the site keeps accounting there.
    pub accounting: Vec<String>,
    /// munged's socket, if `AuthInfo` names one.
    pub munge: Option<PathBuf>,
}

/// Decodes a `RESPONSE_BUILD_INFO` body in `protocol`'s layout.
///
/// # Errors
///
/// Fails when the body is shorter than the layout or a needed value is missing.
pub(crate) fn decode(body: &[u8], protocol: Protocol) -> Result<Config> {
    let u = &mut Unpack::new(body);
    let raw = match protocol {
        Protocol::V23_11 => v23_11(u)?,
        Protocol::V25_11 => v25_11(u)?,
    };
    u.finish()?;
    raw.config()
}

/// The settings glurmo reads, as the controller packed them.
struct Raw {
    accounting_backup: Option<String>,
    accounting_host: Option<String>,
    accounting_port: u16,
    accounting_type: Option<String>,
    auth_info: Option<String>,
    cluster: Option<String>,
    min_job_age: u32,
    private_data: u16,
    slurm_uid: u32,
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
            slurm_uid: self.slurm_uid,
            accounting,
            munge: self.auth_info.as_deref().and_then(munge_socket),
        })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn v23_11(u: &mut Unpack) -> Result<Raw> {
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
    let cluster = u.str()?;
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
    job_defaults(u)?;
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
    plugin_params(u)?;
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
    let version = u.str()?;
    let _vsize_factor = u.u16()?;
    let _wait_time = u.u16()?;
    let _x11_params = u.str()?;
    Ok(Raw {
        accounting_backup,
        accounting_host,
        accounting_port,
        accounting_type,
        auth_info,
        cluster,
        min_job_age,
        private_data,
        slurm_uid,
        slurmctld_params,
        version,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field, generated from 25.11's pack code"
)]
fn v25_11(u: &mut Unpack) -> Result<Raw> {
    let _last_update = u.time()?;
    let _accounting_storage_enforce = u.u16()?;
    let accounting_backup = u.str()?;
    let accounting_host = u.str()?;
    let _accounting_storage_ext_host = u.str()?;
    let _accounting_storage_params = u.str()?;
    let accounting_port = u.u16()?;
    let _accounting_storage_tres = u.str()?;
    let accounting_type = u.str()?;
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
    let _certmgr_params = u.str()?;
    let _certmgr_type = u.str()?;
    key_pairs(u)?;
    let _cli_filter_params = u.str()?;
    let _cli_filter_plugins = u.str()?;
    let cluster = u.str()?;
    let _comm_params = u.str()?;
    let _complete_wait = u.u16()?;
    let _conf_flags = u.u32()?;
    let _control_addr = u.str_array()?;
    let _control_machine = u.str_array()?;
    let _cpu_freq_def = u.u32()?;
    let _cpu_freq_govs = u.u32()?;
    let _cred_type = u.str()?;
    let _data_parser_parameters = u.str()?;
    let _def_mem_per_cpu = u.u64()?;
    let _debug_flags = u.u64()?;
    let _dependency_params = u.str()?;
    let _eio_timeout = u.u16()?;
    let _enforce_part_limits = u.u16()?;
    let _epilog = u.str_array()?;
    let _epilog_msg_time = u.u32()?;
    let _epilog_slurmctld = u.str_array()?;
    let _epilog_timeout = u.u16()?;
    let _fed_params = u.str()?;
    let _first_job_id = u.u32()?;
    let _fs_dampening_factor = u.u16()?;
    let _gres_plugins = u.str()?;
    let _group_time = u.u16()?;
    let _group_force = u.u16()?;
    let _gpu_freq_def = u.str()?;
    let _hash_plugin = u.str()?;
    let _hash_val = u.u32()?;
    let _health_check_interval = u.u16()?;
    let _health_check_node_state = u.u16()?;
    let _health_check_program = u.str()?;
    let _http_parser_type = u.str()?;
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
    let _namespace_plugin = u.str()?;
    job_defaults(u)?;
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
    let _metrics_type = u.str()?;
    let min_job_age = u.u32()?;
    key_pairs(u)?;
    let _mpi_default = u.str()?;
    let _mpi_params = u.str()?;
    let _msg_timeout = u.u16()?;
    let _next_job_id = u.u32()?;
    plugin_params(u)?;
    let _node_features_plugins = u.str()?;
    let _over_time_limit = u.u16()?;
    let _plugindir = u.str()?;
    let _plugstack = u.str()?;
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
    let _priority_weight_age = u.u32()?;
    let _priority_weight_assoc = u.u32()?;
    let _priority_weight_fs = u.u32()?;
    let _priority_weight_js = u.u32()?;
    let _priority_weight_part = u.u32()?;
    let _priority_weight_qos = u.u32()?;
    let _priority_weight_tres = u.str()?;
    let private_data = u.u16()?;
    let _proctrack_type = u.str()?;
    let _prolog = u.str_array()?;
    let _prolog_slurmctld = u.str_array()?;
    let _prolog_timeout = u.u16()?;
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
    let _srun_port_range_0 = u.u16()?;
    let _srun_port_range_1 = u.u16()?;
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
    let _tls_params = u.str()?;
    let _tls_type = u.str()?;
    let _tmp_fs = u.str()?;
    let _topology_param = u.str()?;
    let _topology_plugin = u.str()?;
    let _tree_width = u.u16()?;
    let _unkillable_program = u.str()?;
    let _unkillable_timeout = u.u16()?;
    let _url_parser_type = u.str()?;
    let version = u.str()?;
    let _vsize_factor = u.u16()?;
    let _wait_time = u.u16()?;
    let _x11_params = u.str()?;
    Ok(Raw {
        accounting_backup,
        accounting_host,
        accounting_port,
        accounting_type,
        auth_info,
        cluster,
        min_job_age,
        private_data,
        slurm_uid,
        slurmctld_params,
        version,
    })
}

fn job_defaults(u: &mut Unpack) -> Result<()> {
    u.list(|u| {
        let _type = u.u16()?;
        u.u64().map(drop)
    })
    .map(drop)
}

fn plugin_params(u: &mut Unpack) -> Result<()> {
    u.list(|u| {
        let _name = u.str()?;
        key_pairs(u)
    })
    .map(drop)
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
            slurm_uid: 450,
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
            slurm_uid: 450,
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
