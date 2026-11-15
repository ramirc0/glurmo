//! Rewrites logins, IDs and job names in recorded output, so a recording can become a test fixture.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::fields::is_placeholder;
use super::scontrol::{self, Record};
use super::source::Exchange;
use super::{sacct, squeue};

const ANIMALS: [&str; 24] = [
    "otter", "heron", "lynx", "wren", "ibis", "newt", "vole", "kite", "puma", "yak", "moth",
    "crane", "finch", "gecko", "hare", "mink", "orca", "quail", "seal", "tern", "viper", "wolf",
    "zebu", "bison",
];

/// scontrol keys whose free text can hold paths, project names and comments.
const REDACTED: [&str; 8] = [
    "Command",
    "WorkDir",
    "StdErr",
    "StdIn",
    "StdOut",
    "Comment",
    "AdminComment",
    "SystemComment",
];

/// Stable stand-ins for every login, numeric ID and job name seen so far.
#[derive(Default)]
pub struct Pseudonyms {
    logins: HashMap<String, String>,
    ids: HashMap<String, String>,
    names: HashMap<String, String>,
    hosts: HashMap<String, String>,
    addresses: HashMap<String, String>,
}

impl Pseudonyms {
    /// Rewrites the output of squeue, of scontrol's job listings and config, of sacct and of sdiag.
    /// Other output holds no personal data or site addresses.
    ///
    /// # Errors
    ///
    /// Fails on a squeue line with too few columns.
    pub fn rewrite(&mut self, exchange: &mut Exchange) -> Result<()> {
        let program = exchange.argv.first().map_or("", String::as_str);
        let rewritten = if exchange.argv == squeue::argv() {
            self.squeue(&exchange.stdout)?
        } else if exchange.argv.starts_with(&scontrol::jobs_argv()) {
            self.jobs(&exchange.stdout)
        } else if exchange.argv == scontrol::config_argv() {
            self.config(&exchange.stdout)
        } else if program == "sacct" {
            let history = exchange.argv.contains(&"-S".to_owned());
            self.sacct(&exchange.stdout, history)
        } else if program == "sdiag" {
            self.sdiag(&exchange.stdout)
        } else {
            return Ok(());
        };
        exchange.stdout = rewritten;
        Ok(())
    }

    fn login(&mut self, login: &str) -> String {
        let next = self.logins.len();
        self.logins
            .entry(login.to_owned())
            .or_insert_with(|| {
                ANIMALS
                    .get(next)
                    .map_or_else(|| format!("user{next}"), |animal| (*animal).to_owned())
            })
            .clone()
    }

    fn id(&mut self, id: &str) -> String {
        let next = self.ids.len();
        self.ids
            .entry(id.to_owned())
            .or_insert_with(|| (20_001 + next).to_string())
            .clone()
    }

    fn name(&mut self, name: &str) -> String {
        let next = self.names.len();
        self.names
            .entry(name.to_owned())
            .or_insert_with(|| format!("job{}", next + 1))
            .clone()
    }

    fn host(&mut self, host: &str) -> String {
        let next = self.hosts.len();
        self.hosts
            .entry(host.to_owned())
            .or_insert_with(|| format!("host{}", next + 1))
            .clone()
    }

    /// Maps an address into 192.0.2.0/24, which RFC 5737 sets aside for documentation.
    fn address(&mut self, address: &str) -> String {
        let next = self.addresses.len();
        self.addresses
            .entry(address.to_owned())
            .or_insert_with(|| format!("192.0.2.{}", next + 1))
            .clone()
    }

    /// A submit host's hostname. An address stays for [`Pseudonyms::addresses_in`] to rewrite.
    fn node(&mut self, node: &str) -> String {
        if is_ipv4(node) {
            node.to_owned()
        } else {
            self.host(node)
        }
    }

    /// Rewrites every IPv4 address in `text`.
    fn addresses_in(&mut self, text: &str) -> String {
        map_runs(
            text,
            |c| !(c.is_ascii_digit() || c == '.'),
            |run| {
                if is_ipv4(run) {
                    self.address(run)
                } else {
                    run.to_owned()
                }
            },
        )
    }

    /// Rewrites the controller's hostnames under keys such as `SlurmctldHost[0]` and wherever else they
    /// appear, then every IPv4 address.
    fn config(&mut self, stdout: &str) -> String {
        let mut out = String::new();
        for line in stdout.lines() {
            let line = match line.split_once(" = ") {
                Some((key, value)) if names_hosts(key.trim_end()) && !is_placeholder(value) => {
                    let value = map_runs(
                        value,
                        |c| matches!(c, '(' | ')' | ','),
                        |run| {
                            if run.is_empty() || run == "localhost" || is_ipv4(run) {
                                run.to_owned()
                            } else {
                                self.host(run)
                            }
                        },
                    );
                    format!("{key} = {value}")
                }
                _ => line.to_owned(),
            };
            out.push_str(&self.addresses_in(&line));
            out.push('\n');
        }
        for (host, pseudonym) in &self.hosts {
            out = replace_word(&out, host, pseudonym);
        }
        out
    }

    /// Rewrites `login(id)`, as scontrol prints owners and groups.
    fn owner(&mut self, owner: &str) -> String {
        match owner
            .strip_suffix(')')
            .and_then(|head| head.split_once('('))
        {
            Some((login, id)) => format!("{}({})", self.login(login), self.id(id)),
            None => self.login(owner),
        }
    }

    fn squeue(&mut self, stdout: &str) -> Result<String> {
        let login = column("UserName");
        let name = column("Name");
        let mut out = String::new();
        for line in stdout.lines() {
            let mut columns: Vec<String> = line
                .splitn(squeue::FIELDS.len(), '|')
                .map(str::to_owned)
                .collect();
            if columns.len() != squeue::FIELDS.len() {
                bail!("squeue line {line:?} has too few columns");
            }
            columns[login] = self.login(&columns[login]);
            columns[name] = self.name(&columns[name]);
            out.push_str(&columns.join("|"));
            out.push('\n');
        }
        Ok(out)
    }

    fn jobs(&mut self, stdout: &str) -> String {
        let mut out = String::new();
        for line in stdout.lines() {
            let record = Record::parse(line);
            let mut rewritten = String::new();
            let mut copied = 0;
            for (key, span) in record.spans() {
                let value = &line[span.clone()];
                let replacement = match *key {
                    "JobName" => self.name(value),
                    "UserId" | "GroupId" => self.owner(value),
                    "AllocNode:Sid" => match value.split_once(':') {
                        Some((node, sid)) => format!("{}:{sid}", self.node(node)),
                        None => self.node(value),
                    },
                    key if REDACTED.contains(&key) => "redacted".to_owned(),
                    _ => continue,
                };
                rewritten.push_str(&line[copied..span.start]);
                rewritten.push_str(&replacement);
                copied = span.end;
            }
            rewritten.push_str(&line[copied..]);
            for (login, pseudonym) in &self.logins {
                rewritten = replace_word(&rewritten, login, pseudonym);
            }
            out.push_str(&self.addresses_in(&rewritten));
            out.push('\n');
        }
        out
    }

    /// Rewrites the UID in states such as `CANCELLED by 1514701009`, and the login in history rows.
    fn sacct(&mut self, stdout: &str, history: bool) -> String {
        let login = sacct::HISTORY_FIELDS
            .iter()
            .position(|field| *field == "User")
            .expect("sacct::HISTORY_FIELDS lists User");
        let mut out = String::new();
        for line in stdout.lines() {
            let mut columns: Vec<String> = line.split('|').map(str::to_owned).collect();
            if history && let Some(column) = columns.get_mut(login) {
                *column = self.login(column);
            }
            for column in &mut columns {
                if let Some((state, uid)) = column.split_once(" by ") {
                    *column = format!("{state} by {}", self.id(uid));
                }
            }
            out.push_str(&columns.join("|"));
            out.push('\n');
        }
        out
    }

    /// Rewrites the rows under "Remote Procedure Call statistics by user", such as `\tlogin (1001) count:5 …`.
    fn sdiag(&mut self, stdout: &str) -> String {
        let mut out = String::new();
        let mut by_user = false;
        for line in stdout.lines() {
            if !line.starts_with('\t') {
                by_user = line.starts_with("Remote Procedure Call statistics by user");
            }
            let row = line
                .split_once(')')
                .and_then(|(head, tail)| Some((head.split_once('(')?, tail)));
            let rewritten = match row {
                Some(((login, id), tail)) if by_user => {
                    let login = self.login(login.trim());
                    let id = self.id(id.trim());
                    format!("\t{login:<15} ({id:>8}){tail}")
                }
                _ => line.to_owned(),
            };
            out.push_str(&rewritten);
            out.push('\n');
        }
        out
    }
}

/// Whether a config key holds hostnames, as `AccountingStorageHost` and `SlurmctldHost[0]` do.
fn names_hosts(key: &str) -> bool {
    key.ends_with("Host") || key.ends_with("Addr") || key.starts_with("SlurmctldHost[")
}

fn is_ipv4(text: &str) -> bool {
    text.split('.').count() == 4 && text.split('.').all(|part| part.parse::<u8>().is_ok())
}

/// Passes each run of `text` between separators through `rewrite`, keeping the separators.
fn map_runs(
    text: &str,
    is_separator: impl Fn(char) -> bool,
    mut rewrite: impl FnMut(&str) -> String,
) -> String {
    let mut out = String::new();
    for piece in text.split_inclusive(&is_separator) {
        let run = piece.trim_end_matches(&is_separator);
        out.push_str(&rewrite(run));
        out.push_str(&piece[run.len()..]);
    }
    out
}

fn column(field: &str) -> usize {
    squeue::FIELDS
        .iter()
        .position(|name| *name == field)
        .unwrap_or_else(|| panic!("squeue::FIELDS lacks {field}"))
}

/// Replaces `word` where no letter, digit or underscore touches it.
fn replace_word(text: &str, word: &str, replacement: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(word) {
        out.push_str(&rest[..i]);
        let after = &rest[i + word.len()..];
        let bounded = !out.ends_with(is_word) && !after.starts_with(is_word);
        out.push_str(if bounded { replacement } else { word });
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UnixTime;

    fn exchange(argv: Vec<String>, stdout: &str) -> Exchange {
        Exchange {
            at: UnixTime(0),
            argv,
            status: 0,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn rewrite(pseudonyms: &mut Pseudonyms, argv: Vec<String>, stdout: &str) -> String {
        let mut exchange = exchange(argv, stdout);
        pseudonyms.rewrite(&mut exchange).unwrap();
        exchange.stdout
    }

    #[test]
    fn squeue_rewrites_only_login_and_name() {
        let mut pseudonyms = Pseudonyms::default();
        let out = rewrite(
            &mut pseudonyms,
            squeue::argv(),
            "1|1|N/A|alice|long|RUNNING|2|cpu=2|1:00|5|5|9|z1|None|secret | project\n\
             2|2|N/A|bob|long|RUNNING|2|cpu=2|1:00|5|5|9|z1|None|x\n\
             3|3|N/A|alice|long|PENDING|2|cpu=2|1:00|5|N/A|N/A||Priority|secret | project\n",
        );
        assert_eq!(
            out,
            "1|1|N/A|otter|long|RUNNING|2|cpu=2|1:00|5|5|9|z1|None|job1\n\
             2|2|N/A|heron|long|RUNNING|2|cpu=2|1:00|5|5|9|z1|None|job2\n\
             3|3|N/A|otter|long|PENDING|2|cpu=2|1:00|5|N/A|N/A||Priority|job1\n"
        );
    }

    #[test]
    fn scontrol_rewrites_owners_names_and_paths_but_not_placement() {
        let mut pseudonyms = Pseudonyms::default();
        rewrite(
            &mut pseudonyms,
            squeue::argv(),
            "1|1|N/A|alice|long|RUNNING|2|cpu=2|1:00|5|5|9|z1|None|x\n",
        );
        let line = "JobId=1 JobName=my secret job UserId=alice(1514701041) GroupId=alice(1514701041) Account=alice JobState=RUNNING NumCPUs=2 AllocNode:Sid=10.10.16.29:3050840 JOB_GRES=(null) Nodes=z1 CPU_IDs=0-1 Mem=16384 GRES= MinCPUsNode=1 Command=/home/alice/run me.sh WorkDir=/data/alice/project StdIn=/dev/null";
        let out = rewrite(&mut pseudonyms, scontrol::jobs_argv(), line);
        assert_eq!(
            out.trim_end(),
            "JobId=1 JobName=job2 UserId=otter(20001) GroupId=otter(20001) Account=otter JobState=RUNNING NumCPUs=2 AllocNode:Sid=192.0.2.1:3050840 JOB_GRES=(null) Nodes=z1 CPU_IDs=0-1 Mem=16384 GRES= MinCPUsNode=1 Command=redacted WorkDir=redacted StdIn=redacted"
        );
        let line = "JobId=2 JobName=x UserId=alice(1514701041) AllocNode:Sid=login3:77 Nodes=z1";
        let out = rewrite(&mut pseudonyms, scontrol::jobs_argv(), line);
        assert!(out.contains(" AllocNode:Sid=host1:77 "), "{out}");
    }

    #[test]
    fn sdiag_rewrites_user_rows_only() {
        let mut pseudonyms = Pseudonyms::default();
        let out = rewrite(
            &mut pseudonyms,
            vec!["sdiag".to_owned()],
            "Remote Procedure Call statistics by message type\n\
             \tREQUEST_PING                            ( 1008) count:66588  ave_time:212    total_time:14149021\n\
             \n\
             Remote Procedure Call statistics by user\n\
             \talice           (1514701009) count:286522 ave_time:2794   total_time:800594690\n\
             \troot            (       0) count:152323 ave_time:749    total_time:114197858\n",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("\tREQUEST_PING                            ( 1008)"));
        assert_eq!(
            lines[4],
            "\totter           (   20001) count:286522 ave_time:2794   total_time:800594690"
        );
        assert!(lines[5].starts_with("\theron           (   20002) count:152323"));
    }

    #[test]
    fn sacct_rewrites_the_uid_that_cancelled_a_job() {
        let mut pseudonyms = Pseudonyms::default();
        let out = rewrite(
            &mut pseudonyms,
            crate::slurm::sacct::argv(&[]),
            "1|5|9|CANCELLED by 1514701009\n2|5|9|COMPLETED\n",
        );
        assert_eq!(out, "1|5|9|CANCELLED by 20001\n2|5|9|COMPLETED\n");
    }

    #[test]
    fn sacct_history_rewrites_logins() {
        let mut pseudonyms = Pseudonyms::default();
        let argv = sacct::history_argv(UnixTime(0), UnixTime(60)).unwrap();
        let out = rewrite(
            &mut pseudonyms,
            argv,
            "1|1|realname|4hours|CANCELLED by 77|2\n",
        );
        assert_eq!(out, "1|1|otter|4hours|CANCELLED by 20001|2\n");
    }

    #[test]
    fn config_rewrites_controller_hosts_and_addresses() {
        let mut pseudonyms = Pseudonyms::default();
        let out = rewrite(
            &mut pseudonyms,
            scontrol::config_argv(),
            "\
AccountingStorageBackupHost = (null)
AccountingStorageHost   = 10.7.17.211
ClusterName             = zlurm
JobCompHost             = localhost
SlurmctldAddr           = (null)
SlurmctldHost[0]        = slurmd-211(10.7.17.211)
SlurmctldHost[1]        = slurmd-212
SomeNewKey              = reach 10.7.17.99 here

Slurmctld(primary) at slurmd-211 is UP
",
        );
        assert_eq!(
            out,
            "\
AccountingStorageBackupHost = (null)
AccountingStorageHost   = 192.0.2.1
ClusterName             = zlurm
JobCompHost             = localhost
SlurmctldAddr           = (null)
SlurmctldHost[0]        = host1(192.0.2.1)
SlurmctldHost[1]        = host2
SomeNewKey              = reach 192.0.2.2 here

Slurmctld(primary) at host1 is UP
"
        );
    }

    #[test]
    fn other_commands_pass_through() {
        let mut pseudonyms = Pseudonyms::default();
        let out = rewrite(&mut pseudonyms, vec!["sinfo".to_owned()], "z1|gpu|idle\n");
        assert_eq!(out, "z1|gpu|idle\n");
    }

    #[test]
    fn words_must_stand_alone() {
        assert_eq!(
            replace_word("/home/al/al_x al-1 pal al", "al", "otter"),
            "/home/otter/al_x otter-1 pal otter"
        );
    }
}
