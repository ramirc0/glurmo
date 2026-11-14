//! Slurm's compressed lists of hosts and integers.

use anyhow::{Context, Result, bail};

use crate::model::NodeName;

/// Expands a hostlist such as `z[018-020,1060],js029` into hostnames, in order.
///
/// Placeholders such as `(null)` and the empty string expand to no hosts.
///
/// # Errors
///
/// Fails on an unclosed bracket, text after a bracket, or a bad range.
pub fn expand_hosts(hostlist: &str) -> Result<Vec<NodeName>> {
    let mut hosts = Vec::new();
    let mut rest = hostlist;
    if matches!(rest, "" | "(null)" | "None assigned") {
        return Ok(hosts);
    }
    while !rest.is_empty() {
        let end = rest.find([',', '[']).unwrap_or(rest.len());
        let prefix = &rest[..end];
        rest = &rest[end..];
        if let Some(after) = rest.strip_prefix('[') {
            let (ranges, after) = after
                .split_once(']')
                .with_context(|| format!("unclosed bracket in hostlist {hostlist:?}"))?;
            for item in ranges.split(',') {
                let width = item.find('-').unwrap_or(item.len());
                let (low, high) = range(item)?;
                for number in low..=high {
                    hosts.push(NodeName(format!("{prefix}{number:0width$}")));
                }
            }
            rest = after;
        } else {
            hosts.push(NodeName(prefix.to_owned()));
        }
        rest = match rest.strip_prefix(',') {
            Some(after) => after,
            None if rest.is_empty() => rest,
            None => bail!("unexpected {rest:?} in hostlist {hostlist:?}"),
        };
    }
    Ok(hosts)
}

/// Expands integer ranges such as `0-3,8` into `[0, 1, 2, 3, 8]`.
///
/// # Errors
///
/// Fails on anything but integers and ranges between commas, or on a range that runs backward.
pub fn expand_ids(ranges: &str) -> Result<Vec<u32>> {
    let mut ids = Vec::new();
    for item in ranges.split(',').filter(|item| !item.is_empty()) {
        let (low, high) = range(item)?;
        ids.extend(low..=high);
    }
    Ok(ids)
}

/// Counts the integers that ranges such as `0-3,8` cover, without expanding them.
///
/// # Errors
///
/// Fails on anything but integers and ranges between commas, or on a range that runs backward.
pub fn count_ids(ranges: &str) -> Result<u32> {
    let mut count = 0;
    for item in ranges.split(',').filter(|item| !item.is_empty()) {
        let (low, high) = range(item)?;
        count += high - low + 1;
    }
    Ok(count)
}

fn range(item: &str) -> Result<(u32, u32)> {
    let (low, high) = item.split_once('-').unwrap_or((item, item));
    let (low, high) = (parse_u32(low)?, parse_u32(high)?);
    if high < low {
        bail!("backward range {item:?}");
    }
    Ok((low, high))
}

fn parse_u32(text: &str) -> Result<u32> {
    text.parse()
        .with_context(|| format!("expected an integer, got {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(hostlist: &str) -> Vec<String> {
        expand_hosts(hostlist)
            .unwrap()
            .into_iter()
            .map(|NodeName(name)| name)
            .collect()
    }

    #[test]
    fn hosts_keep_zero_padding_and_order() {
        assert_eq!(
            names("z[018-020,1060],js029"),
            ["z018", "z019", "z020", "z1060", "js029"]
        );
    }

    #[test]
    fn single_hosts_and_placeholders() {
        assert_eq!(names("z1054"), ["z1054"]);
        assert_eq!(names(""), Vec::<String>::new());
        assert_eq!(names("(null)"), Vec::<String>::new());
    }

    #[test]
    fn several_bracket_groups() {
        assert_eq!(names("a[1-2],b[09-10]"), ["a1", "a2", "b09", "b10"]);
    }

    #[test]
    fn malformed_hostlists_fail() {
        assert!(expand_hosts("z[1-2").is_err());
        assert!(expand_hosts("z[1-2]x").is_err());
        assert!(expand_hosts("z[a-b]").is_err());
    }

    #[test]
    fn ids_expand_ranges_and_singles() {
        assert_eq!(expand_ids("0-3,8").unwrap(), [0, 1, 2, 3, 8]);
        assert_eq!(expand_ids("").unwrap(), Vec::<u32>::new());
        assert!(expand_ids("0-x").is_err());
        assert!(expand_ids("108-1").is_err());
    }

    #[test]
    fn counting_avoids_expanding_huge_arrays() {
        assert_eq!(count_ids("0-499999").unwrap(), 500_000);
        assert_eq!(count_ids("0-3,5,7-9").unwrap(), 8);
        assert!(count_ids("9-1").is_err());
    }
}
