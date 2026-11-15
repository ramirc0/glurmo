//! Full names from the system's account database. `getent` runs as its own process, because a static
//! musl binary cannot load the NSS modules that reach SSSD or LDAP.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

use rtviz_model::Login;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Logins per `getent` call, to keep each command line short.
const BATCH: usize = 200;

/// The full names `getent passwd` knows for `logins`. A login without one is left out.
///
/// # Errors
///
/// Fails when `getent` cannot run or runs past the timeout.
pub async fn full_names(logins: &[Login]) -> Result<HashMap<Login, String>> {
    let mut names = HashMap::new();
    for batch in logins.chunks(BATCH) {
        let output = Command::new("getent")
            .arg("passwd")
            .args(batch.iter().map(|login| &login.0))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        // getent exits 2 when any login is unknown, yet still prints the known ones.
        let output = tokio::time::timeout(TIMEOUT, output)
            .await
            .context("getent passwd ran past its timeout")?
            .context("cannot run getent")?;
        names.extend(parse(&String::from_utf8_lossy(&output.stdout)));
    }
    Ok(names)
}

/// Reads each `passwd` line's login and the full name at the start of its GECOS field.
fn parse(passwd: &str) -> impl Iterator<Item = (Login, String)> + '_ {
    passwd.lines().filter_map(|line| {
        let mut fields = line.split(':');
        let login = fields.next()?;
        let gecos = fields.nth(3)?;
        let name = gecos.split(',').next()?.trim();
        (!name.is_empty()).then(|| (Login(login.to_owned()), name.to_owned()))
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
        let names: HashMap<Login, String> = parse(passwd).collect();
        assert_eq!(
            names,
            HashMap::from([(Login("otter".into()), "Sea Otter".into())])
        );
    }
}
