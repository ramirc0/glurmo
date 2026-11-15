//! Logins and full names from the system's account database. `getent` runs as its own process, because a static
//! musl binary cannot load the NSS modules that reach SSSD or LDAP.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

use glurmo_model::Login;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Logins per `getent` call, to keep each command line short.
const BATCH: usize = 200;

/// The full names `getent passwd` knows for `logins`. A login without one is left out.
///
/// # Errors
///
/// Fails when `getent` cannot run or runs past the timeout.
pub async fn full_names(logins: &[Login]) -> Result<HashMap<Login, String>> {
    let keys: Vec<&str> = logins.iter().map(|login| login.0.as_str()).collect();
    Ok(entries(&keys)
        .await?
        .into_iter()
        .filter_map(|entry| Some((entry.login, entry.full_name?)))
        .collect())
}

/// The logins `getent passwd` knows for `uids`. A UID without one is left out.
///
/// # Errors
///
/// Fails when `getent` cannot run or runs past the timeout.
pub async fn logins(uids: &[u32]) -> Result<HashMap<u32, Login>> {
    let keys: Vec<String> = uids.iter().map(u32::to_string).collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    Ok(entries(&keys)
        .await?
        .into_iter()
        .map(|entry| (entry.uid, entry.login))
        .collect())
}

struct Entry {
    login: Login,
    uid: u32,
    full_name: Option<String>,
}

/// Looks up logins or UIDs.
async fn entries(keys: &[&str]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for batch in keys.chunks(BATCH) {
        let output = Command::new("getent")
            .arg("passwd")
            .args(batch)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        // getent exits 2 when any key is unknown, yet still prints the known ones.
        let output = tokio::time::timeout(TIMEOUT, output)
            .await
            .context("getent passwd ran past its timeout")?
            .context("cannot run getent")?;
        entries.extend(parse(&String::from_utf8_lossy(&output.stdout)));
    }
    Ok(entries)
}

/// Reads each `passwd` line's login, UID, and the full name at the start of its GECOS field.
fn parse(passwd: &str) -> impl Iterator<Item = Entry> + '_ {
    passwd.lines().filter_map(|line| {
        let mut fields = line.split(':');
        let login = fields.next()?;
        let uid = fields.nth(1)?.parse().ok()?;
        let gecos = fields.nth(1)?;
        let name = gecos.split(',').next()?.trim();
        Some(Entry {
            login: Login(login.to_owned()),
            uid,
            full_name: (!name.is_empty()).then(|| name.to_owned()),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_name_is_the_first_gecos_field() {
        let passwd = "otter:*:1001:1001:Sea Otter,Room 4,,:/home/otter:/bin/bash\n\
                      heron:*:1002:1002::/home/heron:/bin/sh\n\
                      svc:*:1003:1003:,,,:/srv:/sbin/nologin\n";
        let entries: Vec<Entry> = parse(passwd).collect();
        let names: HashMap<&Login, &str> = entries
            .iter()
            .filter_map(|entry| Some((&entry.login, entry.full_name.as_deref()?)))
            .collect();
        assert_eq!(
            names,
            HashMap::from([(&Login("otter".into()), "Sea Otter")])
        );
        let uids: Vec<u32> = entries.iter().map(|entry| entry.uid).collect();
        assert_eq!(uids, [1001, 1002, 1003]);
    }
}
