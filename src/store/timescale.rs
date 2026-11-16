//! The TimescaleDB side of the connection layer: a PostgreSQL client on a thread of its own.

use std::cell::Cell;
use std::collections::HashMap;
use std::error::Error;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::BytesMut;
use postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use postgres::{Client, Config, NoTls, Statement};

use super::connection::Value;

/// What a call reports once the client's thread has gone.
const STOPPED: &str = "the TimescaleDB client's thread has stopped";

/// What a call reports while the server has not answered an earlier one.
const UNANSWERED: &str = "the TimescaleDB server has stopped answering";

/// Server settings that bound a call, unless the connection string's `options` set them. A lock held
/// elsewhere fails a statement after 30 s, and any statement fails after 5 minutes. The first prune of
/// a year of compressed history is the slowest statement the store runs.
const LIMITS: [(&str, &str); 2] = [("lock_timeout", "30s"), ("statement_timeout", "5min")];

/// How long a call waits for its answer before it checks that the server still answers. A stopped
/// server, or one stuck on its disk, answers nothing and fails no timeout of its own.
const PATIENCE: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 30 });

/// How long that check waits for a query on a new connection.
const PROBE: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 10 });

/// Reads a connection string, such as `host=db user=glurmo dbname=glurmo` or a `postgres://` URL.
/// Unless it sets them, connecting gives up after 10 s, and over TCP, a server that stops
/// acknowledging data, or a host that stops answering keepalives, fails a call within a minute. The
/// server bounds lock waits and statements as [`LIMITS`] says. So a dead server or a held lock cannot
/// hang its caller.
///
/// # Errors
///
/// Fails when `url` does not parse.
pub fn parse(url: &str) -> Result<Config> {
    let mut config: Config = url
        .parse()
        .context("the TimescaleDB connection string does not parse")?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(10));
    }
    if config.get_tcp_user_timeout().is_none() {
        config.tcp_user_timeout(Duration::from_secs(30));
    }
    // The parsed idle time has a default, so only the string tells whether it was set.
    if !url.contains("keepalives_idle") {
        config.keepalives_idle(Duration::from_secs(30));
    }
    if config.get_keepalives_interval().is_none() {
        config.keepalives_interval(Duration::from_secs(10));
    }
    if config.get_keepalives_retries().is_none() {
        config.keepalives_retries(3);
    }
    let mut options = config.get_options().unwrap_or_default().to_owned();
    for (setting, limit) in LIMITS {
        if !options.contains(setting) {
            options = format!("{options} -c {setting}={limit}");
        }
    }
    config.options(options.trim_start());
    Ok(config)
}

/// A connection to a TimescaleDB server.
///
/// `postgres::Client` blocks on a runtime of its own in every call and when it drops, and tokio
/// panics when that happens on one of its workers. The sampler calls the store from a worker. So the
/// client lives on a thread of its own, and each call here hands that thread a closure and waits for
/// its answer. The thread ends, and the client drops on it, when this value drops.
///
/// A call gives up on a server that stops answering, and later calls fail at once until the thread
/// has finished the call that was given up on. A transaction it was in is rolled back then.
pub struct Timescale {
    work: mpsc::Sender<Work>,
    config: Box<Config>,
    /// Hears once the thread has finished the calls given up on.
    behind: Cell<Option<mpsc::Receiver<()>>>,
    /// Whether a transaction began and has not ended.
    in_transaction: Cell<bool>,
}

/// One call, run on the client's thread.
type Work = Box<dyn FnOnce(&mut Session) + Send>;

impl Timescale {
    /// Connects to the server `config` names.
    ///
    /// # Errors
    ///
    /// Fails when the thread cannot start or the server cannot be reached.
    pub fn connect(config: &Config) -> Result<Self> {
        let (work, queue) = mpsc::channel::<Work>();
        let (connected, outcome) = mpsc::sync_channel(1);
        let session_config = config.clone();
        std::thread::Builder::new()
            .name("timescaledb".to_owned())
            .spawn(move || {
                let mut session = match Session::open(session_config) {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = connected.send(Err(error));
                        return;
                    }
                };
                let _ = connected.send(Ok(()));
                for work in queue {
                    work(&mut session);
                }
            })?;
        outcome
            .recv_timeout(PATIENCE)
            .map_err(|_| anyhow!(UNANSWERED))??;
        Ok(Self {
            work,
            config: Box::new(config.clone()),
            behind: Cell::new(None),
            in_transaction: Cell::new(false),
        })
    }

    /// Runs `work` on the client's thread and waits for its answer. A panic in `work` fails this call
    /// only. The client stays usable, because it skips the rest of an answer that nobody waits for.
    /// After [`PATIENCE`], a call that a probe finds the server unable to answer is given up on.
    fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Session) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if let Some(caught_up) = self.behind.take() {
            if let Err(mpsc::TryRecvError::Empty) = caught_up.try_recv() {
                self.behind.set(Some(caught_up));
                bail!(UNANSWERED);
            }
            tracing::info!("the TimescaleDB client has finished the calls it gave up on");
        }
        let (answer, answered) = mpsc::sync_channel(1);
        self.send(Box::new(move |session| {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| work(session)))
                .unwrap_or_else(|_| Err(anyhow!("a TimescaleDB call panicked")));
            let _ = answer.send(outcome);
        }))?;
        loop {
            match answered.recv_timeout(PATIENCE) {
                Ok(outcome) => return outcome,
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!(STOPPED),
                Err(mpsc::RecvTimeoutError::Timeout) if answers(&self.config) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.in_transaction.replace(false) {
                        self.send(Box::new(Session::roll_back))?;
                    }
                    let (caught_up, behind) = mpsc::sync_channel(1);
                    self.send(Box::new(move |_| {
                        let _ = caught_up.send(());
                    }))?;
                    self.behind.set(Some(behind));
                    tracing::warn!(
                        "the TimescaleDB server answered neither a call within {PATIENCE:?} nor a probe within {PROBE:?}, so calls fail until the client finishes it"
                    );
                    bail!(UNANSWERED);
                }
            }
        }
    }

    fn send(&self, work: Work) -> Result<()> {
        self.work.send(work).ok().context(STOPPED)
    }

    /// Runs one statement and returns how many rows it changed.
    ///
    /// # Errors
    ///
    /// Fails when the server refuses the statement or cannot be reached.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.call(move |session| {
            let statement = session.statement(&sql)?;
            let changed = session.client.execute(&statement, &borrowed(&params))?;
            Ok(usize::try_from(changed)?)
        })
    }

    /// Runs a query and returns the rows of its answer.
    ///
    /// # Errors
    ///
    /// Fails when the server refuses the query or cannot be reached.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<postgres::Row>> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.call(move |session| {
            let statement = session.statement(&sql)?;
            Ok(session.client.query(&statement, &borrowed(&params))?)
        })
    }

    /// Runs statements separated by semicolons, with no parameters.
    ///
    /// # Errors
    ///
    /// Fails when the server refuses a statement or cannot be reached.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let sql = sql.to_owned();
        self.call(move |session| {
            session.reconnect_between_transactions()?;
            Ok(session.client.batch_execute(&sql)?)
        })
    }

    /// Starts a transaction.
    ///
    /// # Errors
    ///
    /// Fails when the server cannot be reached.
    pub fn begin(&self) -> Result<()> {
        self.in_transaction.set(true);
        let begun = self.call(|session| {
            session.reconnect_between_transactions()?;
            session.client.batch_execute("BEGIN")?;
            session.in_transaction = true;
            Ok(())
        });
        if begun.is_err() {
            self.in_transaction.set(false);
        }
        begun
    }

    /// Ends the transaction: commits it when `commit`, else rolls it back.
    ///
    /// # Errors
    ///
    /// Fails when the server refuses or cannot be reached, or the transaction was rolled back when the
    /// server stopped answering. The transaction is over either way.
    pub fn end(&self, commit: bool) -> Result<()> {
        if !self.in_transaction.replace(false) {
            bail!("the transaction was rolled back when the TimescaleDB server stopped answering");
        }
        self.call(move |session| {
            session.in_transaction = false;
            let ending = if commit { "COMMIT" } else { "ROLLBACK" };
            Ok(session.client.batch_execute(ending)?)
        })
    }
}

/// The client and what it remembers between calls. It lives on the client's thread.
struct Session {
    config: Config,
    client: Client,
    /// Prepared statements, by their SQL in SQLite's style.
    statements: HashMap<String, Statement>,
    in_transaction: bool,
}

impl Session {
    fn open(config: Config) -> Result<Self> {
        Ok(Self {
            client: connect(&config)?,
            config,
            statements: HashMap::new(),
            in_transaction: false,
        })
    }

    /// Connects again when the server has dropped the connection, unless a transaction is open. The
    /// server rolled that one back, and on a new connection its remaining statements would commit
    /// without its first ones.
    fn reconnect_between_transactions(&mut self) -> Result<()> {
        if self.client.is_closed() && !self.in_transaction {
            self.client = connect(&self.config)?;
            self.statements.clear();
            tracing::info!("reconnected to the TimescaleDB server");
        }
        Ok(())
    }

    /// Rolls back a transaction that the caller gave up on. A server that has dropped the connection
    /// rolled it back already.
    fn roll_back(&mut self) {
        self.in_transaction = false;
        let _ = self.client.batch_execute("ROLLBACK");
    }

    /// The prepared statement for `sql`, which is prepared at its first use.
    fn statement(&mut self, sql: &str) -> Result<Statement> {
        self.reconnect_between_transactions()?;
        if let Some(statement) = self.statements.get(sql) {
            return Ok(statement.clone());
        }
        let statement = self.client.prepare(&dollar_numbered(sql))?;
        self.statements.insert(sql.to_owned(), statement.clone());
        Ok(statement)
    }
}

/// Whether the server answers a query on a new connection within [`PROBE`]. A probe that gets no
/// answer is left to finish on its own thread.
fn answers(config: &Config) -> bool {
    let (answer, answered) = mpsc::sync_channel(1);
    let config = config.clone();
    let probe = std::thread::Builder::new()
        .name("timescaledb-probe".to_owned())
        .spawn(move || {
            let answered = connect(&config).and_then(|mut client| {
                client.batch_execute("SELECT 1")?;
                Ok(())
            });
            let _ = answer.send(answered.is_ok());
        });
    probe.is_ok() && answered.recv_timeout(PROBE).unwrap_or(false)
}

fn connect(config: &Config) -> Result<Client> {
    let mut client = config
        .connect(NoTls)
        .context("cannot connect to TimescaleDB")?;
    // TimescaleDB refuses a statement that decompresses more than 100,000 rows, as a guard against
    // mistakes typed by hand. Pruning a year of history or backfilling over old chunks can exceed it.
    client.batch_execute("SET timescaledb.max_tuples_decompressed_per_dml_transaction = 0")?;
    Ok(client)
}

fn borrowed(params: &[Value]) -> Vec<&(dyn ToSql + Sync)> {
    params
        .iter()
        .map(|value| value as &(dyn ToSql + Sync))
        .collect()
}

impl ToSql for Value {
    fn to_sql(
        &self,
        kind: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match self {
            Self::Null => Ok(IsNull::Yes),
            Self::Integer(number) => number.to_sql_checked(kind, out),
            Self::Text(text) => text.to_sql_checked(kind, out),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Reads the column at `index` of `row`, counted from 0.
///
/// # Errors
///
/// Fails when the row has no such column, or its type is not `BIGINT`, `TEXT` or `BOOLEAN`.
pub fn value(row: &postgres::Row, index: usize) -> Result<Value> {
    let Some(column) = row.columns().get(index) else {
        bail!("the row has no column {index}");
    };
    Ok(match *column.type_() {
        Type::INT8 => row.try_get::<_, Option<i64>>(index)?.into(),
        Type::TEXT => row.try_get::<_, Option<String>>(index)?.into(),
        Type::BOOL => row.try_get::<_, Option<bool>>(index)?.into(),
        ref other => bail!("column {index} holds a {other}"),
    })
}

/// Rewrites SQLite's `?1` parameters as PostgreSQL's `$1`. Quoted text and names are left alone.
fn dollar_numbered(sql: &str) -> String {
    let mut quote = None;
    let mut rewritten = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(open) if c == open => quote = None,
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '?' && chars.peek().is_some_and(char::is_ascii_digit) => {
                rewritten.push('$');
                continue;
            }
            Some(_) | None => {}
        }
        rewritten.push(c);
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use postgres::config::Host;

    use super::*;

    #[test]
    fn numbered_parameters_become_dollars_outside_quotes() {
        assert_eq!(
            dollar_numbered(r#"UPDATE t SET "end" = ?3 WHERE job = ?1 AND submit = ?12"#),
            r#"UPDATE t SET "end" = $3 WHERE job = $1 AND submit = $12"#
        );
        assert_eq!(
            dollar_numbered(r#"SELECT '?1', 'it''s ?2', "?3", ?4 FROM t"#),
            r#"SELECT '?1', 'it''s ?2', "?3", $4 FROM t"#
        );
        assert_eq!(dollar_numbered("SELECT 'a\"b', ?1"), "SELECT 'a\"b', $1");
        assert_eq!(dollar_numbered("SELECT '?' || ?1"), "SELECT '?' || $1");
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_connection_the_server_drops_comes_back_between_transactions_only() {
        let server = std::env::var("GLURMO_TEST_TIMESCALE").unwrap();
        let db = Timescale::connect(&parse(&server).unwrap()).unwrap();
        let hang_up = "SELECT pg_terminate_backend(pg_backend_pid())";
        let rows = |db: &Timescale| db.query("SELECT 1", &[]).map(|rows| rows.len());

        assert!(db.execute_batch(hang_up).is_err());
        // The client may learn that the connection closed only from the next call.
        let _ = rows(&db);
        assert_eq!(rows(&db).unwrap(), 1, "a new connection");

        db.begin().unwrap();
        assert!(db.execute_batch(hang_up).is_err());
        assert!(rows(&db).is_err());
        assert!(rows(&db).is_err(), "no new connection inside a transaction");
        assert!(db.end(true).is_err(), "the commit fails with it");
        assert_eq!(rows(&db).unwrap(), 1, "a new connection once it is over");
    }

    #[test]
    fn a_connection_string_keeps_its_own_timeouts_and_gets_the_missing_ones() {
        let config = parse(
            "host=/run/postgresql dbname=glurmo connect_timeout=3 keepalives_idle=600 \
             options='-c search_path=otter -c lock_timeout=1s'",
        )
        .unwrap();
        assert_eq!(config.get_connect_timeout(), Some(&Duration::from_secs(3)));
        assert_eq!(
            config.get_tcp_user_timeout(),
            Some(&Duration::from_secs(30))
        );
        assert_eq!(config.get_keepalives_idle(), Duration::from_secs(600));
        assert_eq!(
            config.get_options(),
            Some("-c search_path=otter -c lock_timeout=1s -c statement_timeout=5min")
        );

        let config = parse("host=db dbname=glurmo").unwrap();
        assert_eq!(config.get_keepalives_idle(), Duration::from_secs(30));
        assert_eq!(
            config.get_keepalives_interval(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(config.get_keepalives_retries(), Some(3));
        assert_eq!(
            config.get_options(),
            Some("-c lock_timeout=30s -c statement_timeout=5min")
        );
        assert!(parse("host=db port=five").is_err());
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_call_that_panics_fails_alone() {
        let server = std::env::var("GLURMO_TEST_TIMESCALE").unwrap();
        let db = Timescale::connect(&parse(&server).unwrap()).unwrap();
        let panicked = db.call(|_| -> Result<()> { panic!("a bug") }).unwrap_err();
        assert!(format!("{panicked:#}").contains("panicked"), "{panicked:#}");
        assert_eq!(db.query("SELECT 1", &[]).unwrap().len(), 1);
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_statement_that_runs_too_long_fails() {
        let server = std::env::var("GLURMO_TEST_TIMESCALE").unwrap();
        let db = Timescale::connect(&parse(&server).unwrap()).unwrap();
        let limit: Vec<postgres::Row> = db.query("SHOW statement_timeout", &[]).unwrap();
        assert_eq!(limit[0].get::<_, String>(0), "5min");
        db.execute_batch("SET statement_timeout = '100ms'").unwrap();
        let slept = db.execute_batch("SELECT pg_sleep(5)").unwrap_err();
        assert!(
            format!("{slept:#}").contains("statement timeout"),
            "{slept:#}"
        );
    }

    /// A Unix socket that relays connections to the test server, and the switch that freezes it. A
    /// frozen relay accepts connections and passes nothing on, as a stopped server does.
    struct Relay {
        dir: tempfile::TempDir,
        port: u16,
        frozen: Arc<AtomicBool>,
    }

    impl Relay {
        fn new() -> Self {
            let server = parse(&std::env::var("GLURMO_TEST_TIMESCALE").unwrap()).unwrap();
            let Some(Host::Unix(server_dir)) = server.get_hosts().first() else {
                panic!("the relay needs a server on a Unix socket");
            };
            let port = server.get_ports().first().copied().unwrap_or(5432);
            let socket = format!(".s.PGSQL.{port}");
            let target = server_dir.join(&socket);
            let dir = tempfile::tempdir().unwrap();
            let listener = UnixListener::bind(dir.path().join(&socket)).unwrap();
            let frozen = Arc::new(AtomicBool::new(false));
            let switch = Arc::clone(&frozen);
            std::thread::spawn(move || {
                for client in listener.incoming().flatten() {
                    let server = UnixStream::connect(&target).unwrap();
                    let back = (server.try_clone().unwrap(), client.try_clone().unwrap());
                    for (from, to) in [(client, server), back] {
                        let frozen = Arc::clone(&switch);
                        std::thread::spawn(move || pass_on(from, to, &frozen));
                    }
                }
            });
            Self { dir, port, frozen }
        }

        fn config(&self) -> Config {
            parse(&format!(
                "host={} port={} dbname=glurmo",
                self.dir.path().display(),
                self.port
            ))
            .unwrap()
        }

        fn freeze(&self, frozen: bool) {
            self.frozen.store(frozen, Ordering::SeqCst);
        }
    }

    fn pass_on(mut from: UnixStream, mut to: UnixStream, frozen: &AtomicBool) {
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = from.read(&mut buffer) {
            while frozen.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if to.write_all(&buffer[..read]).is_err() {
                return;
            }
        }
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_server_that_stops_answering_fails_calls_and_rolls_back_once_it_answers() {
        let relay = Relay::new();
        let db = Timescale::connect(&relay.config()).unwrap();
        db.execute_batch("CREATE TEMP TABLE t (x BIGINT)").unwrap();
        db.begin().unwrap();
        db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();

        relay.freeze(true);
        let frozen_at = Instant::now();
        assert!(db.execute("INSERT INTO t VALUES (2)", &[]).is_err());
        assert!(frozen_at.elapsed() < PATIENCE + PROBE + Duration::from_secs(1));
        let failed_at = Instant::now();
        assert!(db.query("SELECT 1", &[]).is_err());
        assert!(db.end(true).is_err(), "no commit");
        assert!(failed_at.elapsed() < Duration::from_millis(100), "at once");

        relay.freeze(false);
        let rows = (0..100)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(50));
                db.query("SELECT x FROM t", &[]).ok()
            })
            .expect("calls work once the server answers");
        assert_eq!(rows.len(), 0, "the transaction rolled back");
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_slow_statement_on_a_server_that_answers_runs_to_the_end() {
        let relay = Relay::new();
        let db = Timescale::connect(&relay.config()).unwrap();
        let sleep = format!("SELECT pg_sleep({})", 2 * PATIENCE.as_secs());
        db.execute_batch(&sleep).unwrap();
    }
}
