//! Logins, full names and groups from the system's account database, found as `passwd: files sss` and
//! `group: files sss` would find them: `/etc/passwd` and `/etc/group` first, then SSSD's NSS responder
//! over its socket. A static musl binary cannot load NSS modules, so glurmo speaks SSSD's protocol
//! itself. Without SSSD it falls back to `getent` and `id`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Command;

use glurmo_model::{GroupName, Login};

const TIMEOUT: Duration = Duration::from_secs(15);

/// Keys per `getent` call, to keep each command line short.
const BATCH: usize = 200;

const SSS_NSS: &str = "/var/lib/sss/pipes/nss";

/// The full names the account database knows for `logins`. A login without one is left out.
///
/// # Errors
///
/// Fails when SSSD or `getent` fails or runs past the timeout.
pub async fn full_names(logins: &[Login]) -> Result<HashMap<Login, String>> {
    let keys: Vec<Key> = logins.iter().map(|login| Key::Login(&login.0)).collect();
    Ok(entries(&keys)
        .await?
        .into_iter()
        .filter_map(|entry| Some((entry.login, entry.full_name?)))
        .collect())
}

/// The logins the account database knows for `uids`. A UID without one is left out.
///
/// # Errors
///
/// Fails when SSSD or `getent` fails or runs past the timeout.
pub async fn logins(uids: &[u32]) -> Result<HashMap<u32, Login>> {
    let keys: Vec<Key> = uids.iter().copied().map(Key::Uid).collect();
    Ok(entries(&keys)
        .await?
        .into_iter()
        .map(|entry| (entry.uid, entry.login))
        .collect())
}

/// The groups the account database puts each of `logins` in, by name: the primary one and every one that
/// lists the login as a member, less a private one named after the login. A login the database does
/// not know has none.
///
/// # Errors
///
/// Fails when SSSD or `id` fails or runs past the timeout.
pub async fn groups(logins: &[Login]) -> Result<HashMap<Login, Vec<GroupName>>> {
    let local = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let socket = Path::new(SSS_NSS);
    if !socket.exists() {
        let mut found = HashMap::new();
        for login in logins {
            found.insert(login.clone(), shared(login, id(login).await?));
        }
        return Ok(found);
    }
    let keys: Vec<Key> = logins.iter().map(|login| Key::Login(&login.0)).collect();
    let primary: HashMap<Login, u32> = entries(&keys)
        .await?
        .into_iter()
        .map(|entry| (entry.login, entry.gid))
        .collect();
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut found = HashMap::new();
    let lookups = async {
        let mut stream = connect(socket).await?;
        for login in logins {
            let mut member_of: Vec<String> = Vec::new();
            let mut gids = gids_reply(
                &request(&mut stream, INITGR, &[login.0.as_bytes(), b"\0"].concat()).await?,
            )?;
            gids.extend(primary.get(login));
            for line in local.lines() {
                let Some((name, gid, members)) = group_line(line) else {
                    continue;
                };
                names.entry(gid).or_insert_with(|| name.to_owned());
                if members.split(',').any(|member| member == login.0) {
                    member_of.push(name.to_owned());
                }
            }
            for gid in gids {
                let name = if let Some(name) = names.get(&gid) {
                    name.clone()
                } else {
                    let reply = request(&mut stream, GETGRGID, &gid.to_ne_bytes()).await?;
                    let name = group_reply(&reply)?.unwrap_or_else(|| gid.to_string());
                    names.insert(gid, name.clone());
                    name
                };
                member_of.push(name);
            }
            found.insert(login.clone(), shared(login, member_of));
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(TIMEOUT, lookups)
        .await
        .context("SSSD ran past its timeout")??;
    Ok(found)
}

/// `member_of` as group names in name order, each once, without the private group named after `login`.
fn shared(login: &Login, member_of: Vec<String>) -> Vec<GroupName> {
    let names: BTreeSet<String> = member_of
        .into_iter()
        .filter(|name| *name != login.0)
        .collect();
    names.into_iter().map(GroupName).collect()
}

/// A line of `/etc/group` as the group's name, its GID and its comma-separated members.
fn group_line(line: &str) -> Option<(&str, u32, &str)> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    let gid = fields.nth(1)?.parse().ok()?;
    Some((name, gid, fields.next().unwrap_or_default()))
}

/// Asks `id -Gn`, for sites whose groups come from NSS modules other than SSSD. An unknown login has
/// no groups.
async fn id(login: &Login) -> Result<Vec<String>> {
    let output = Command::new("id")
        .args(["-Gn", "--", &login.0])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIMEOUT, output)
        .await
        .context("id ran past its timeout")?
        .context("cannot run id")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

/// What to look an account up by.
#[derive(Clone, Copy)]
enum Key<'a> {
    Uid(u32),
    Login(&'a str),
}

impl Key<'_> {
    fn finds(self, entry: &Entry) -> bool {
        match self {
            Key::Uid(uid) => entry.uid == uid,
            Key::Login(login) => entry.login.0 == login,
        }
    }
}

struct Entry {
    login: Login,
    uid: u32,
    gid: u32,
    full_name: Option<String>,
}

/// Looks keys up in `/etc/passwd`, then asks SSSD, or `getent` where SSSD does not run, for the rest.
async fn entries(keys: &[Key<'_>]) -> Result<Vec<Entry>> {
    let files = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let mut found: Vec<Entry> = parse(&files)
        .filter(|entry| keys.iter().any(|key| key.finds(entry)))
        .collect();
    let missing: Vec<Key> = keys
        .iter()
        .copied()
        .filter(|key| !found.iter().any(|entry| key.finds(entry)))
        .collect();
    if missing.is_empty() {
        return Ok(found);
    }
    let socket = Path::new(SSS_NSS);
    let others = if socket.exists() {
        tokio::time::timeout(TIMEOUT, sssd(socket, &missing))
            .await
            .context("SSSD ran past its timeout")??
    } else {
        getent(&missing).await?
    };
    found.extend(others);
    Ok(found)
}

const GET_VERSION: u32 = 0x0001;
const GETPWNAM: u32 = 0x0011;
const GETPWUID: u32 = 0x0012;
const GETGRGID: u32 = 0x0022;
const INITGR: u32 = 0x0026;
const NSS_PROTOCOL: u32 = 1;
const HEADER: usize = 16;

/// Asks SSSD's NSS responder for each key, as `libnss_sss` does.
async fn sssd(socket: &Path, keys: &[Key<'_>]) -> Result<Vec<Entry>> {
    let mut stream = connect(socket).await?;
    let mut entries = Vec::new();
    for key in keys {
        let (command, body) = match *key {
            Key::Uid(uid) => (GETPWUID, uid.to_ne_bytes().to_vec()),
            Key::Login(login) => (GETPWNAM, [login.as_bytes(), b"\0"].concat()),
        };
        let reply = request(&mut stream, command, &body).await?;
        entries.extend(passwd_reply(&reply)?);
    }
    Ok(entries)
}

/// Opens SSSD's socket and checks that it speaks the protocol glurmo knows.
async fn connect(socket: &Path) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("cannot reach SSSD at {}", socket.display()))?;
    let version = request(&mut stream, GET_VERSION, &NSS_PROTOCOL.to_ne_bytes()).await?;
    if version.get(..4) != Some(&NSS_PROTOCOL.to_ne_bytes()[..]) {
        bail!("SSSD speaks another NSS protocol");
    }
    Ok(stream)
}

/// Sends one request and returns its reply's body. SSSD frames both with four host-order words:
/// the total length, the command, a status and a reserved word.
async fn request(stream: &mut UnixStream, command: u32, body: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(HEADER + body.len())?;
    let mut message = Vec::with_capacity(HEADER + body.len());
    for word in [len, command, 0, 0] {
        message.extend(word.to_ne_bytes());
    }
    message.extend(body);
    stream.write_all(&message).await?;
    let mut header = [0; HEADER];
    stream.read_exact(&mut header).await?;
    let word = |i: usize| u32::from_ne_bytes(header[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
    let (len, replied, status) = (word(0) as usize, word(1), word(2));
    if replied != command {
        bail!("SSSD answered command {command:#x} with {replied:#x}");
    }
    if status != 0 {
        bail!("SSSD failed command {command:#x} with error {status}");
    }
    let mut reply = vec![0; len.checked_sub(HEADER).context("short SSSD reply")?];
    stream.read_exact(&mut reply).await?;
    Ok(reply)
}

/// Reads a passwd reply: a count, a reserved word, then the UID, the GID, and the login, password,
/// GECOS, home and shell as NUL-terminated strings. A count of zero means no such account.
fn passwd_reply(reply: &[u8]) -> Result<Option<Entry>> {
    let word = |i: usize| -> Result<u32> {
        let bytes = reply.get(i * 4..i * 4 + 4).context("short passwd reply")?;
        Ok(u32::from_ne_bytes(bytes.try_into()?))
    };
    if word(0)? == 0 {
        return Ok(None);
    }
    let (uid, gid) = (word(2)?, word(3)?);
    let mut strings = reply
        .get(16..)
        .context("short passwd reply")?
        .split(|&byte| byte == 0);
    let mut next = || strings.next().map(String::from_utf8_lossy);
    let login = next()
        .filter(|login| !login.is_empty())
        .context("a passwd reply without a login")?
        .into_owned();
    let _password = next();
    let gecos = next().unwrap_or_default();
    Ok(Some(Entry {
        login: Login(login),
        uid,
        gid,
        full_name: full_name(&gecos),
    }))
}

/// Reads an initgroups reply: a count, a reserved word, then that many GIDs.
fn gids_reply(reply: &[u8]) -> Result<Vec<u32>> {
    let mut words = reply
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_ne_bytes);
    let count = words.next().context("short initgroups reply")? as usize;
    let gids: Vec<u32> = words.skip(1).take(count).collect();
    if gids.len() != count {
        bail!(
            "an initgroups reply with {} of its {count} groups",
            gids.len()
        );
    }
    Ok(gids)
}

/// Reads a group reply for its name: a count, a reserved word, the GID, the member count, then the
/// name, password and members as NUL-terminated strings. A count of zero means no such group.
fn group_reply(reply: &[u8]) -> Result<Option<String>> {
    let count = reply.get(..4).context("short group reply")?;
    if u32::from_ne_bytes(count.try_into()?) == 0 {
        return Ok(None);
    }
    let name = reply
        .get(16..)
        .and_then(|strings| strings.split(|&byte| byte == 0).next())
        .filter(|name| !name.is_empty())
        .context("a group reply without a name")?;
    Ok(Some(String::from_utf8_lossy(name).into_owned()))
}

/// Asks `getent passwd`, for sites whose accounts come from NSS modules other than SSSD.
async fn getent(keys: &[Key<'_>]) -> Result<Vec<Entry>> {
    let arguments: Vec<String> = keys
        .iter()
        .map(|key| match *key {
            Key::Uid(uid) => uid.to_string(),
            Key::Login(login) => login.to_owned(),
        })
        .collect();
    let mut entries = Vec::new();
    for batch in arguments.chunks(BATCH) {
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

/// Reads each `passwd` line's login, UID and full name.
fn parse(passwd: &str) -> impl Iterator<Item = Entry> + '_ {
    passwd.lines().filter_map(|line| {
        let mut fields = line.split(':');
        let login = fields.next()?;
        let uid = fields.nth(1)?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        let gecos = fields.next()?;
        Some(Entry {
            login: Login(login.to_owned()),
            uid,
            gid,
            full_name: full_name(gecos),
        })
    })
}

/// The full name at the start of a GECOS field.
fn full_name(gecos: &str) -> Option<String> {
    let name = gecos.split(',').next()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
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

    #[test]
    fn sssd_passwd_replies_hold_one_account_or_none() {
        let mut reply = Vec::new();
        for word in [1_u32, 0, 1001, 1001] {
            reply.extend(word.to_ne_bytes());
        }
        reply.extend(b"otter\0*\0Sea Otter,Room 4\0/home/otter\0/bin/bash\0");
        let entry = passwd_reply(&reply).unwrap().unwrap();
        assert_eq!(
            (
                entry.login.0.as_str(),
                entry.uid,
                entry.full_name.as_deref()
            ),
            ("otter", 1001, Some("Sea Otter"))
        );
        let none: Vec<u8> = [0_u32, 0].iter().flat_map(|w| w.to_ne_bytes()).collect();
        assert!(passwd_reply(&none).unwrap().is_none());
    }

    #[test]
    fn short_sssd_passwd_replies_are_errors() {
        for words in [&[1_u32, 0, 1001][..], &[1, 0, 1001, 1001]] {
            let reply: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
            assert!(passwd_reply(&reply).is_err(), "{} bytes", reply.len());
        }
    }

    #[test]
    fn group_lines_give_the_name_the_gid_and_the_members() {
        assert_eq!(
            group_line("gpujobs:x:1003:otter,heron"),
            Some(("gpujobs", 1003, "otter,heron"))
        );
        assert_eq!(group_line("adm:x:4:"), Some(("adm", 4, "")));
        assert_eq!(group_line("broken:x"), None);
    }

    #[test]
    fn a_private_group_is_left_out_and_the_rest_come_in_name_order() {
        let member_of = ["mohd", "otter", "encode", "mohd"]
            .map(str::to_owned)
            .to_vec();
        let names: Vec<String> = shared(&Login("otter".into()), member_of)
            .into_iter()
            .map(|group| group.0)
            .collect();
        assert_eq!(names, ["encode", "mohd"]);
    }

    #[test]
    fn sssd_initgroups_replies_list_gids() {
        let words =
            |words: &[u32]| -> Vec<u8> { words.iter().flat_map(|w| w.to_ne_bytes()).collect() };
        assert_eq!(
            gids_reply(&words(&[2, 0, 10000, 10002])).unwrap(),
            [10000, 10002]
        );
        assert_eq!(gids_reply(&words(&[0, 0])).unwrap(), [0_u32; 0]);
        assert!(gids_reply(&words(&[2, 0, 10000])).is_err());
        assert!(gids_reply(&[]).is_err());
    }

    #[test]
    fn sssd_group_replies_hold_one_name_or_none() {
        let mut reply: Vec<u8> = [1_u32, 0, 10002, 2]
            .iter()
            .flat_map(|w| w.to_ne_bytes())
            .collect();
        reply.extend(b"encode\0*\0otter\0heron\0");
        assert_eq!(group_reply(&reply).unwrap().as_deref(), Some("encode"));
        let none: Vec<u8> = [0_u32, 0].iter().flat_map(|w| w.to_ne_bytes()).collect();
        assert_eq!(group_reply(&none).unwrap(), None);
        assert!(group_reply(&reply[..16]).is_err());
        assert!(group_reply(&[]).is_err());
    }

    /// Run on a host where `id` reads the same account database, as on js029 and AICR.
    #[tokio::test]
    #[ignore = "needs the host's account database"]
    async fn groups_match_id() {
        let me = Login(std::env::var("USER").unwrap());
        let found = groups(std::slice::from_ref(&me)).await.unwrap();
        assert_eq!(found[&me], shared(&me, id(&me).await.unwrap()));
        assert_ne!(found[&me], []);
        let nobody = Login("no-such-login-here".into());
        assert_eq!(
            groups(std::slice::from_ref(&nobody)).await.unwrap()[&nobody],
            []
        );
    }

    #[tokio::test]
    async fn sssd_knows_this_user() {
        let socket = Path::new(SSS_NSS);
        if !socket.exists() {
            eprintln!("no SSSD here");
            return;
        }
        let uid = std::os::unix::fs::MetadataExt::uid(
            &std::fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap(),
        );
        let entries = sssd(socket, &[Key::Uid(uid), Key::Uid(u32::MAX - 7)])
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uid, uid);
        let by_name = sssd(socket, &[Key::Login(&entries[0].login.0)])
            .await
            .unwrap();
        assert_eq!(by_name[0].uid, uid);
    }
}
