//! Parsers for values that several Slurm commands print the same way.

use anyhow::{Context, Result, bail};

use super::hostlist::expand_ids;
use glurmo_model::{EndState, JobState, NodeFlag, NodeState, UnixTime};

/// Builds an `-O` spec such as `JobID:0|,Name:0`.
///
/// Size 0 turns off padding and truncation. The last field gets no `|`, so it can hold free text that contains one.
pub fn format_spec(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| format!("{field}:0"))
        .collect::<Vec<_>>()
        .join("|,")
}

/// Splits one line of `-O` output into exactly `N` columns. The last column keeps any further `|`.
///
/// # Errors
///
/// Fails when the line has fewer than `N` columns.
pub fn columns<const N: usize>(line: &str) -> Result<[&str; N]> {
    let columns: Vec<&str> = line.splitn(N, '|').collect();
    let found = columns.len();
    columns
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected {N} columns, found {found} in {line:?}"))
}

/// Whether Slurm printed a stand-in for a missing value.
pub fn is_placeholder(text: &str) -> bool {
    matches!(
        text,
        "" | "N/A"
            | "(null)"
            | "None"
            | "none"
            | "Unknown"
            | "UNLIMITED"
            | "Partition_Limit"
            | "NOT_SET"
    )
}

/// Parses an integer.
///
/// # Errors
///
/// Fails when the text is not an integer that fits the type.
pub fn integer<T: std::str::FromStr>(text: &str) -> Result<T> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("expected an integer, got {text:?}"))
}

/// Parses `[D-]HH:MM:SS` or `MM:SS` into seconds. Placeholders such as `UNLIMITED` give `None`.
///
/// # Errors
///
/// Fails on any other text.
pub fn seconds(duration: &str) -> Result<Option<u64>> {
    if is_placeholder(duration) {
        return Ok(None);
    }
    let (days, clock) = match duration.split_once('-') {
        Some((days, clock)) => (integer::<u64>(days)?, clock),
        None => (0, duration),
    };
    let parts = clock
        .split(':')
        .map(integer::<u64>)
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("bad duration {duration:?}"))?;
    let (hours, minutes, secs) = match parts[..] {
        [hours, minutes, secs] => (hours, minutes, secs),
        [minutes, secs] if days == 0 => (0, minutes, secs),
        _ => bail!("bad duration {duration:?}"),
    };
    Ok(Some(((days * 24 + hours) * 60 + minutes) * 60 + secs))
}

/// Parses a size such as `16G`, `1000000M`, `62.50G` or a bare MB count into MB.
/// Placeholders give `None`.
///
/// # Errors
///
/// Fails on any other text.
pub fn megabytes(size: &str) -> Result<Option<u64>> {
    if is_placeholder(size) {
        return Ok(None);
    }
    let digits = size.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let scale = match &size[digits.len()..] {
        "K" => 1.0 / 1024.0,
        "" | "M" => 1.0,
        "G" => 1024.0,
        "T" => 1024.0 * 1024.0,
        _ => bail!("bad size {size:?}"),
    };
    let number: f64 = digits
        .parse()
        .with_context(|| format!("bad size {size:?}"))?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(Some((number * scale).round() as u64))
}

/// Parses epoch seconds, as Slurm prints times under `SLURM_TIME_FORMAT=%s`.
/// Placeholders such as `N/A` give `None`.
///
/// # Errors
///
/// Fails on any other text.
pub fn time(text: &str) -> Result<Option<UnixTime>> {
    if is_placeholder(text) {
        return Ok(None);
    }
    Ok(Some(UnixTime(integer(text)?)))
}

/// The memory and GPUs in a TRES string such as `cpu=4,mem=16G,gres/gpu=2`.
#[derive(Debug, PartialEq, Eq)]
pub struct Tres {
    /// Memory.
    pub mem_mb: Option<u64>,
    /// GPUs of any type.
    pub gpus: u32,
}

/// Parses a TRES string. Placeholders give no memory and no GPUs.
///
/// # Errors
///
/// Fails on a bad memory size or GPU count.
pub fn tres(text: &str) -> Result<Tres> {
    let mut tres = Tres {
        mem_mb: None,
        gpus: 0,
    };
    if is_placeholder(text) {
        return Ok(tres);
    }
    for entry in text.split(',') {
        match entry.split_once('=') {
            Some(("mem", size)) => tres.mem_mb = megabytes(size)?,
            Some(("gres/gpu", count)) => tres.gpus = integer(count)?,
            _ => {}
        }
    }
    Ok(tres)
}

/// One entry of a GRES list, such as `gpu:a100:2(IDX:0,2)`.
#[derive(Debug, PartialEq, Eq)]
pub struct Gres<'a> {
    /// The resource, such as `gpu` or `shard`.
    pub name: &'a str,
    /// The type, such as `a100` or a MIG profile, if the site sets one.
    pub kind: Option<&'a str>,
    /// How many.
    pub count: u64,
    /// The text in parentheses, such as `IDX:0,2` or `S:0-1`.
    pub detail: Option<&'a str>,
}

/// Parses a GRES list with the grammar `name[:type][:no_consume]:count[(detail)]`.
/// Details can hold commas, so entries split only on commas outside parentheses.
///
/// # Errors
///
/// Fails on an unclosed parenthesis or a bad count.
pub fn gres(text: &str) -> Result<Vec<Gres<'_>>> {
    if is_placeholder(text) {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .with_context(|| format!("unopened parenthesis in GRES {text:?}"))?;
            }
            ',' if depth == 0 => {
                entries.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth > 0 {
        bail!("unclosed parenthesis in GRES {text:?}");
    }
    entries.push(&text[start..]);
    entries
        .into_iter()
        .map(|entry| gres_entry(entry).with_context(|| format!("bad GRES {text:?}")))
        .collect()
}

fn gres_entry(entry: &str) -> Result<Gres<'_>> {
    let (spec, detail) = match entry.strip_suffix(')') {
        Some(head) => {
            let open = head.rfind('(').context("no opening parenthesis")?;
            (&head[..open], Some(&head[open + 1..]))
        }
        None => (entry, None),
    };
    let mut parts: Vec<&str> = spec.split(':').collect();
    let name = parts.remove(0);
    // A GRES held without a device bitmap prints as `name:type(CNT:n)`, with no count before it.
    let counted = parts
        .last()
        .is_some_and(|last| last.starts_with(|c: char| c.is_ascii_digit()));
    let count = match (
        counted,
        detail.and_then(|detail| detail.strip_prefix("CNT:")),
    ) {
        (true, _) => gres_count(parts.pop().unwrap_or_default())?,
        (false, Some(count)) => integer(count)?,
        (false, None) => 1,
    };
    let kind = parts
        .into_iter()
        .find(|part| *part != "no_consume" && *part != "(null)");
    Ok(Gres {
        name,
        kind,
        count,
        detail,
    })
}

fn gres_count(count: &str) -> Result<u64> {
    let digits = count.trim_end_matches(['K', 'M', 'G', 'T', 'P']);
    let scale: u64 = match &count[digits.len()..] {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        "P" => 1 << 50,
        suffix => bail!("unknown suffix {suffix:?} in GRES count {count:?}"),
    };
    integer::<u64>(digits)?
        .checked_mul(scale)
        .with_context(|| format!("GRES count {count:?} overflows"))
}

/// Sums the GPUs in a parsed GRES list.
pub fn gpu_count(entries: &[Gres]) -> u32 {
    let total: u64 = entries
        .iter()
        .filter(|entry| entry.name == "gpu")
        .map(|entry| entry.count)
        .sum();
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// The GPU type, when every GPU entry names the same one.
pub fn gpu_model(entries: &[Gres]) -> Option<String> {
    let mut kinds = entries
        .iter()
        .filter(|entry| entry.name == "gpu")
        .map(|entry| entry.kind);
    let first = kinds.next()??;
    kinds
        .all(|kind| kind == Some(first))
        .then(|| first.to_owned())
}

/// Collects the GPU indices from `IDX:` details, such as `gpu:2(IDX:0,2)`.
///
/// # Errors
///
/// Fails on a bad index list.
pub fn gpu_indices(entries: &[Gres]) -> Result<Vec<u32>> {
    let mut indices = Vec::new();
    for entry in entries.iter().filter(|entry| entry.name == "gpu") {
        if let Some(ids) = entry.detail.and_then(|detail| detail.strip_prefix("IDX:"))
            && !is_placeholder(ids)
        {
            indices.extend(expand_ids(ids)?);
        }
    }
    Ok(indices)
}

/// Maps a job state as squeue and scontrol print it.
///
/// # Errors
///
/// Fails on a state that Slurm 23.11 through 26.05 does not print.
pub fn job_state(raw: &str) -> Result<JobState> {
    use EndState::{
        Cancelled, Completed, Failed, NodeFail, OutOfMemory, Preempted, Requeued, Timeout,
    };
    Ok(match raw {
        "PENDING" | "REQUEUE_HOLD" | "REQUEUE_FED" | "RESV_DEL_HOLD" => JobState::Pending,
        "RUNNING" | "CONFIGURING" | "RESIZING" | "SIGNALING" | "STOPPED" => JobState::Running,
        "SUSPENDED" => JobState::Suspended,
        "COMPLETING" | "STAGE_OUT" | "EXPEDITING" => JobState::Completing,
        "COMPLETED" => JobState::Ended(Completed),
        "FAILED" | "BOOT_FAIL" | "SPECIAL_EXIT" => JobState::Ended(Failed),
        "CANCELLED" | "REVOKED" => JobState::Ended(Cancelled),
        "TIMEOUT" | "DEADLINE" => JobState::Ended(Timeout),
        "OUT_OF_MEMORY" => JobState::Ended(OutOfMemory),
        "NODE_FAIL" => JobState::Ended(NodeFail),
        "PREEMPTED" => JobState::Ended(Preempted),
        "REQUEUED" => JobState::Ended(Requeued),
        _ => bail!("unknown job state {raw:?}"),
    })
}

/// Maps a `StateComplete` value such as `mixed+drain+not_responding`.
/// Unknown base states map to [`NodeState::Unknown`], and unknown flags stay only in the raw string.
pub fn node_state(raw: &str) -> (NodeState, Vec<NodeFlag>) {
    let mut parts = raw.split('+');
    let base = parts.next().unwrap_or_default();
    let flags: Vec<&str> = parts.collect();
    let draining = flags.iter().any(|flag| matches!(*flag, "drain" | "fail"));
    let state = match base {
        "down" => NodeState::Down,
        "idle" if draining => NodeState::Drained,
        "mixed" | "allocated" if draining => NodeState::Draining,
        "idle" => NodeState::Idle,
        "mixed" => NodeState::Mixed,
        "allocated" => NodeState::Allocated,
        _ => NodeState::Unknown,
    };
    let mut neutral: Vec<NodeFlag> = flags
        .iter()
        .filter_map(|flag| match *flag {
            "not_responding" => Some(NodeFlag::NotResponding),
            "power_down" | "powered_down" | "powering_down" | "powering_up" => {
                Some(NodeFlag::PowerSaving)
            }
            "reserved" => Some(NodeFlag::Reserved),
            "maint" => Some(NodeFlag::Maintenance),
            _ => None,
        })
        .collect();
    neutral.dedup();
    (state, neutral)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(seconds("5:00").unwrap(), Some(300));
        assert_eq!(seconds("12:00:00").unwrap(), Some(43_200));
        assert_eq!(seconds("60-00:00:00").unwrap(), Some(5_184_000));
        assert_eq!(seconds("1-02:03:04").unwrap(), Some(93_784));
        assert_eq!(seconds("UNLIMITED").unwrap(), None);
        assert_eq!(seconds("Partition_Limit").unwrap(), None);
        assert!(seconds("1-02:03").is_err());
        assert!(seconds("soon").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(megabytes("16G").unwrap(), Some(16_384));
        assert_eq!(megabytes("1000000M").unwrap(), Some(1_000_000));
        assert_eq!(megabytes("62.50G").unwrap(), Some(64_000));
        assert_eq!(megabytes("36951420K").unwrap(), Some(36_085));
        assert_eq!(megabytes("0").unwrap(), Some(0));
        assert_eq!(megabytes("N/A").unwrap(), None);
        assert!(megabytes("16Q").is_err());
    }

    #[test]
    fn times_and_placeholders() {
        assert_eq!(time("1794330933").unwrap(), Some(UnixTime(1_794_330_933)));
        for placeholder in ["N/A", "Unknown", "None", "(null)", ""] {
            assert_eq!(time(placeholder).unwrap(), None);
        }
        assert!(time("2026-11-14T17:00:00").is_err());
    }

    #[test]
    fn tres_reads_memory_and_untyped_gpus() {
        let parsed = tres("cpu=32,mem=256G,node=1,billing=32,gres/gpu=4,gres/gpu:h200=4").unwrap();
        assert_eq!(
            parsed,
            Tres {
                mem_mb: Some(262_144),
                gpus: 4
            }
        );
        assert_eq!(tres("(null)").unwrap().gpus, 0);
    }

    #[test]
    fn gres_with_commas_inside_details() {
        let entries = gres("gpu:a100:2(IDX:0,2),gpu:a100:1(IDX:5),shard:8").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(gpu_count(&entries), 3);
        assert_eq!(gpu_indices(&entries).unwrap(), [0, 2, 5]);
        assert_eq!(gpu_model(&entries).as_deref(), Some("a100"));
    }

    #[test]
    fn untyped_and_null_typed_gres() {
        let untyped = gres("gpu:4(IDX:0-3)").unwrap();
        assert_eq!(untyped[0].kind, None);
        assert_eq!(gpu_indices(&untyped).unwrap(), [0, 1, 2, 3]);
        assert_eq!(gpu_model(&untyped), None);

        let null = gres("gpu:(null):0(IDX:N/A)").unwrap();
        assert_eq!((null[0].kind, null[0].count), (None, 0));
        assert_eq!(gpu_indices(&null).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn gres_suffixes_flags_and_shards() {
        let entries = gres("gpumem:no_consume:40G,shard:3(1/5,2/5),gpu:4(S:0-1)").unwrap();
        assert_eq!(entries[0].count, 40 << 30);
        assert_eq!(entries[0].kind, None);
        assert_eq!(entries[1].detail, Some("1/5,2/5"));
        assert_eq!(gpu_count(&entries), 4);
        assert_eq!(gpu_indices(&entries).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn mixed_gpu_types_have_no_single_model() {
        let entries = gres("gpu:a100_3g.20gb:2,gpu:a100:1").unwrap();
        assert_eq!(gpu_model(&entries), None);
        assert_eq!(gpu_count(&entries), 3);
    }

    #[test]
    fn gres_counted_in_detail_and_large_suffixes() {
        let typed = gres("gpu:v100(CNT:2)").unwrap();
        assert_eq!((typed[0].kind, typed[0].count), (Some("v100"), 2));
        let untyped = gres("craynetwork(CNT:1)").unwrap();
        assert_eq!((untyped[0].kind, untyped[0].count), (None, 1));
        assert_eq!(gres("scratch:no_consume:4T").unwrap()[0].count, 4 << 40);
        assert_eq!(gres("bb:2P").unwrap()[0].count, 2 << 50);
    }

    #[test]
    fn bad_gres_fails() {
        assert!(gres("gpu:4(IDX:0-3").is_err());
        assert!(gres("gpu:4x").is_err());
        assert!(gres("gpu:4(IDX:0)x").is_err());
    }

    #[test]
    fn job_states() {
        assert_eq!(job_state("COMPLETING").unwrap(), JobState::Completing);
        assert_eq!(job_state("CONFIGURING").unwrap(), JobState::Running);
        assert_eq!(
            job_state("OUT_OF_MEMORY").unwrap(),
            JobState::Ended(EndState::OutOfMemory)
        );
        assert!(job_state("DANCING").is_err());
    }

    #[test]
    fn neutral_states_cover_draining_and_power_saving() {
        assert_eq!(node_state("mixed+drain"), (NodeState::Draining, Vec::new()));
        assert_eq!(
            node_state("idle+powered_down+reserved+maint"),
            (
                NodeState::Idle,
                vec![
                    NodeFlag::PowerSaving,
                    NodeFlag::Reserved,
                    NodeFlag::Maintenance
                ]
            )
        );
        assert_eq!(node_state("future"), (NodeState::Unknown, Vec::new()));
    }
}
