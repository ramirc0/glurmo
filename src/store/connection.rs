//! One way to run the store's SQL, whichever database holds it.

use anyhow::{Context, Result, bail};
use rusqlite::types::{ToSqlOutput, ValueRef};

/// A value bound to a statement or read from a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// SQL's `NULL`.
    Null,
    /// A whole number. Booleans are stored as 0 and 1.
    Integer(i64),
    /// Text.
    Text(String),
}

impl From<i64> for Value {
    fn from(number: i64) -> Self {
        Self::Integer(number)
    }
}

impl From<u32> for Value {
    fn from(number: u32) -> Self {
        Self::Integer(number.into())
    }
}

impl From<bool> for Value {
    fn from(flag: bool) -> Self {
        Self::Integer(flag.into())
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<&String> for Value {
    fn from(text: &String) -> Self {
        Self::Text(text.clone())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl<T> From<Option<T>> for Value
where
    Value: From<T>,
{
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Self::from)
    }
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Self::Null => ValueRef::Null,
            Self::Integer(number) => ValueRef::Integer(*number),
            Self::Text(text) => ValueRef::Text(text.as_bytes()),
        }))
    }
}

/// The values to bind to a statement's `?1`, `?2` and so on, in that order.
macro_rules! params {
    ($($param:expr),* $(,)?) => {
        &[$($crate::store::connection::Value::from($param)),*]
    };
}
pub(crate) use params;

/// What a column of a row can be read as.
pub trait FromValue: Sized {
    /// Reads `value` as this type.
    ///
    /// # Errors
    ///
    /// Fails when `value` is of another kind, or a number out of this type's range.
    fn from_value(value: Value) -> Result<Self>;
}

impl FromValue for i64 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Integer(number) => Ok(number),
            other => bail!("expected a number, found {other:?}"),
        }
    }
}

impl FromValue for u32 {
    fn from_value(value: Value) -> Result<Self> {
        Ok(Self::try_from(i64::from_value(value)?)?)
    }
}

impl FromValue for u64 {
    fn from_value(value: Value) -> Result<Self> {
        Ok(Self::try_from(i64::from_value(value)?)?)
    }
}

impl FromValue for usize {
    fn from_value(value: Value) -> Result<Self> {
        Ok(Self::try_from(i64::from_value(value)?)?)
    }
}

impl FromValue for bool {
    fn from_value(value: Value) -> Result<Self> {
        Ok(i64::from_value(value)? != 0)
    }
}

impl FromValue for String {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Text(text) => Ok(text),
            other => bail!("expected text, found {other:?}"),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            value => T::from_value(value).map(Some),
        }
    }
}

/// One row of a query's answer.
pub enum Row<'a> {
    /// A row from SQLite.
    Sqlite(&'a rusqlite::Row<'a>),
}

impl Row<'_> {
    /// Reads the column at `index`, counted from 0.
    ///
    /// # Errors
    ///
    /// Fails when the row has no such column, or it does not hold a `T`.
    pub fn get<T: FromValue>(&self, index: usize) -> Result<T> {
        let value = match self {
            Self::Sqlite(row) => match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(number) => Value::Integer(number),
                ValueRef::Text(text) => Value::Text(std::str::from_utf8(text)?.to_owned()),
                other => bail!("column {index} holds a {}", other.data_type()),
            },
        };
        T::from_value(value).with_context(|| format!("column {index}"))
    }
}

/// A connection to the database that holds the store. SQL is written in SQLite's style, with
/// parameters as `?1`, `?2` and so on. Each statement is prepared once and kept.
pub enum Connection {
    /// A SQLite file.
    Sqlite(rusqlite::Connection),
}

impl Connection {
    /// Runs one statement and returns how many rows it changed.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses the statement.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        match self {
            Self::Sqlite(db) => Ok(db
                .prepare_cached(sql)?
                .execute(rusqlite::params_from_iter(params))?),
        }
    }

    /// Runs statements separated by semicolons, with no parameters.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses a statement.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match self {
            Self::Sqlite(db) => Ok(db.execute_batch(sql)?),
        }
    }

    /// Runs a query and reads each row of its answer with `read`.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses the query, or `read` fails on a row.
    pub fn query_map<T>(
        &self,
        sql: &str,
        params: &[Value],
        mut read: impl FnMut(&Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        match self {
            Self::Sqlite(db) => {
                let mut statement = db.prepare_cached(sql)?;
                let mut rows = statement.query(rusqlite::params_from_iter(params))?;
                let mut read_rows = Vec::new();
                while let Some(row) = rows.next()? {
                    read_rows.push(read(&Row::Sqlite(row))?);
                }
                Ok(read_rows)
            }
        }
    }

    /// Runs a query and reads the first row of its answer with `read`.
    ///
    /// # Errors
    ///
    /// Fails when the database refuses the query, the answer has no row, or `read` fails.
    pub fn query_row<T>(
        &self,
        sql: &str,
        params: &[Value],
        read: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<T> {
        let mut read = Some(read);
        let mut first = None;
        self.query_map(sql, params, |row| {
            if let Some(read) = read.take() {
                first = Some(read(row)?);
            }
            Ok(())
        })?;
        first.context("the query returned no rows")
    }

    /// Starts a transaction. It rolls back when dropped, unless [`Transaction::commit`] ran.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot start one.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.execute_batch("BEGIN")?;
        Ok(Transaction {
            db: self,
            open: true,
        })
    }
}

/// A transaction under way. It dereferences to its [`Connection`], which runs the statements.
pub struct Transaction<'a> {
    db: &'a Connection,
    open: bool,
}

impl Transaction<'_> {
    /// Makes the transaction's changes permanent.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot commit. The changes are then rolled back.
    pub fn commit(mut self) -> Result<()> {
        self.db.execute_batch("COMMIT")?;
        self.open = false;
        Ok(())
    }
}

impl std::ops::Deref for Transaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.db
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
}

/// Rewrites SQLite's `?1` parameters as PostgreSQL's `$1`. Quoted text and names are left alone.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the TimescaleDB backend will call it")
)]
pub fn dollar_numbered(sql: &str) -> String {
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
    use super::*;

    fn memory() -> Connection {
        Connection::Sqlite(rusqlite::Connection::open_in_memory().unwrap())
    }

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
    fn values_come_from_what_the_store_binds() {
        assert_eq!(Value::from(-7_i64), Value::Integer(-7));
        assert_eq!(Value::from(u32::MAX), Value::Integer(4_294_967_295));
        assert_eq!(Value::from(true), Value::Integer(1));
        assert_eq!(Value::from("otter"), Value::Text("otter".into()));
        assert_eq!(
            Value::from(&"heron".to_owned()),
            Value::Text("heron".into())
        );
        assert_eq!(Value::from(None::<u32>), Value::Null);
        assert_eq!(Value::from(Some("0-3")), Value::Text("0-3".into()));
    }

    #[test]
    fn values_read_back_as_the_type_asked_for() {
        assert_eq!(i64::from_value(Value::Integer(-7)).unwrap(), -7);
        assert_eq!(u32::from_value(Value::Integer(8)).unwrap(), 8);
        assert_eq!(u64::from_value(Value::Integer(8)).unwrap(), 8);
        assert_eq!(usize::from_value(Value::Integer(8)).unwrap(), 8);
        assert!(bool::from_value(Value::Integer(1)).unwrap());
        assert!(!bool::from_value(Value::Integer(0)).unwrap());
        assert_eq!(
            String::from_value(Value::Text("otter".into())).unwrap(),
            "otter"
        );
        assert_eq!(Option::<i64>::from_value(Value::Null).unwrap(), None);
        assert_eq!(
            Option::<String>::from_value(Value::Text("up".into())).unwrap(),
            Some("up".to_owned())
        );
    }

    #[test]
    fn a_value_of_the_wrong_kind_or_range_is_an_error() {
        assert!(u32::from_value(Value::Integer(-1)).is_err());
        assert!(u32::from_value(Value::Integer(1 << 32)).is_err());
        assert!(i64::from_value(Value::Null).is_err());
        assert!(i64::from_value(Value::Text("7".into())).is_err());
        assert!(String::from_value(Value::Integer(7)).is_err());
    }

    #[test]
    fn values_round_trip_through_sqlite() {
        let db = memory();
        db.execute_batch("CREATE TABLE t (number INTEGER, text TEXT, missing TEXT)")
            .unwrap();
        let changed = db
            .execute(
                "INSERT INTO t VALUES (?1, ?2, ?3)",
                params![i64::MIN, "it's ?1", None::<&str>],
            )
            .unwrap();
        assert_eq!(changed, 1);
        let row: (i64, String, Option<String>) = db
            .query_row("SELECT number, text, missing FROM t", params![], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(row, (i64::MIN, "it's ?1".to_owned(), None));
        let wrong = db.query_row("SELECT text FROM t", params![], |row| row.get::<i64>(0));
        assert!(format!("{:#}", wrong.unwrap_err()).contains("column 0"));
    }

    #[test]
    fn a_query_with_no_rows_fails_query_row_and_gives_query_map_nothing() {
        let db = memory();
        db.execute_batch("CREATE TABLE t (number INTEGER)").unwrap();
        let read = |row: &Row| row.get::<i64>(0);
        assert!(
            db.query_row("SELECT number FROM t", params![], read)
                .is_err()
        );
        let rows = db
            .query_map("SELECT number FROM t", params![], read)
            .unwrap();
        assert_eq!(rows, Vec::<i64>::new());
    }

    #[test]
    fn a_transaction_dropped_before_its_commit_rolls_back() {
        let mut db = memory();
        db.execute_batch("CREATE TABLE t (number INTEGER)").unwrap();
        let count = |db: &Connection| -> i64 {
            db.query_row("SELECT count(*) FROM t", params![], |row| row.get(0))
                .unwrap()
        };
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO t VALUES (?1)", params![1_i64])
            .unwrap();
        drop(tx);
        assert_eq!(count(&db), 0);
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO t VALUES (?1)", params![2_i64])
            .unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&db), 1);
    }
}
