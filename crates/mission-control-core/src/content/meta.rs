//! Meta for posts, users and terms, and site options: named values stored as JSON text, after
//! WordPress's `get_post_meta`/`update_post_meta`/`delete_post_meta` and `get_option`/
//! `update_option`/`delete_option`.
//!
//! One implementation, [`Store`], serves the four tables; the public functions are thin wrappers
//! that name the table. Each `set_*` replaces the value stored under the key and inserts it when
//! there is none, as `update_post_meta` does. Each `set_*_raw` takes the value as text and refuses
//! text that is not valid JSON, for forms and the importer.
//!
//! ```ignore
//! use mission_control_core::content::{get_post_meta, set_post_meta};
//!
//! set_post_meta(&db, post.id, "subtitle", &serde_json::json!("Hello")).await?;
//! let subtitle = get_post_meta(&db, post.id, "subtitle").await?;
//! ```

use std::fmt;

use orbit_orm::{Database, Error, Value as Sql, raw};
use serde_json::Value;

use super::taxonomy::Conn;

/// Why a meta value or option was not read or written.
#[derive(Debug)]
pub enum MetaError {
    /// The text is not valid JSON: given to a `set_*_raw` function, or found stored in a row.
    InvalidJson(serde_json::Error),
    /// The database refused a query, for example a post that does not exist.
    Database(Error),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(e) => write!(f, "the value is not valid JSON: {e}"),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for MetaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidJson(e) => Some(e),
            Self::Database(e) => Some(e),
        }
    }
}

impl From<Error> for MetaError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

impl From<serde_json::Error> for MetaError {
    fn from(e: serde_json::Error) -> Self {
        Self::InvalidJson(e)
    }
}

/// A table of named values: the columns that hold the owner, the key and the value.
struct Store {
    table: &'static str,
    /// The column that names the owning row (`post_id`), or `None` when the table is global.
    owner: Option<&'static str>,
    key: &'static str,
    value: &'static str,
}

const POST_META: Store = Store {
    table: "post_meta",
    owner: Some("post_id"),
    key: "meta_key",
    value: "meta_value",
};
const USER_META: Store = Store {
    table: "user_meta",
    owner: Some("user_id"),
    key: "meta_key",
    value: "meta_value",
};
const TERM_META: Store = Store {
    table: "term_meta",
    owner: Some("term_id"),
    key: "meta_key",
    value: "meta_value",
};
const OPTIONS: Store = Store {
    table: "options",
    owner: None,
    key: "name",
    value: "value",
};

impl Store {
    /// The `WHERE` clause that picks `key` of `owner`, and its binds.
    fn filter(&self, owner: i64, key: &str) -> (String, Vec<Sql>) {
        match self.owner {
            Some(column) => (
                format!("{column} = ? AND {} = ?", self.key),
                vec![owner.into(), key.into()],
            ),
            None => (format!("{} = ?", self.key), vec![key.into()]),
        }
    }

    /// The value under `key`: the first row, if the key repeats.
    async fn get<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
    ) -> Result<Option<Value>, MetaError> {
        let (filter, binds) = self.filter(owner, key);
        let sql = format!(
            "SELECT {} FROM {} WHERE {filter} ORDER BY id LIMIT 1",
            self.value, self.table
        );
        let text: Option<(String,)> = raw::fetch_optional(c.exec(), &sql, &binds).await?;
        text.map(|(text,)| serde_json::from_str(&text).map_err(MetaError::from))
            .transpose()
    }

    /// Replaces the text under `key`, or inserts it when the key is absent. The text is stored as
    /// it is, so callers pass JSON. Not atomic by itself: run it in a transaction.
    async fn put<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<(), Error> {
        let (filter, binds) = self.filter(owner, key);
        // Existence is counted rather than read from the rows an `UPDATE` changed: MySQL reports
        // no changed rows when the value is the same, which would insert a duplicate.
        let found: i64 = raw::fetch_scalar(
            c.exec(),
            &format!("SELECT COUNT(*) FROM {} WHERE {filter}", self.table),
            &binds,
        )
        .await?;
        if found > 0 {
            let mut binds = binds;
            binds.insert(0, json.into());
            raw::execute(
                c.exec(),
                &format!(
                    "UPDATE {} SET {} = ? WHERE {filter}",
                    self.table, self.value
                ),
                &binds,
            )
            .await?;
        } else {
            self.insert(c, owner, key, json).await?;
        }
        Ok(())
    }

    /// Inserts a row under `key` with the text `json`, whatever rows the key already has.
    async fn insert<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<(), Error> {
        let (columns, marks, binds) = match self.owner {
            Some(column) => (
                format!("{column}, {}, {}", self.key, self.value),
                "?, ?, ?",
                vec![owner.into(), key.into(), json.into()],
            ),
            None => (
                format!("{}, {}", self.key, self.value),
                "?, ?",
                vec![key.into(), json.into()],
            ),
        };
        raw::execute(
            c.exec(),
            &format!("INSERT INTO {} ({columns}) VALUES ({marks})", self.table),
            &binds,
        )
        .await?;
        Ok(())
    }

    /// [`put`](Self::put) in its own transaction.
    async fn set_text(
        &self,
        db: &Database,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<(), MetaError> {
        db.transaction(async |tx| self.put(tx, owner, key, json).await)
            .await?;
        Ok(())
    }

    /// Stores `value` as JSON text.
    async fn set(
        &self,
        db: &Database,
        owner: i64,
        key: &str,
        value: &Value,
    ) -> Result<(), MetaError> {
        self.set_text(db, owner, key, &value.to_string()).await
    }

    /// Stores `json` after checking that it parses, so a bad value never reaches the table.
    async fn set_raw(
        &self,
        db: &Database,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<(), MetaError> {
        serde_json::from_str::<Value>(json)?;
        self.set_text(db, owner, key, json).await
    }

    /// Every value under `key`, in the order the rows were added.
    async fn get_all<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
    ) -> Result<Vec<Value>, MetaError> {
        let (filter, binds) = self.filter(owner, key);
        let sql = format!(
            "SELECT {} FROM {} WHERE {filter} ORDER BY id",
            self.value, self.table
        );
        let rows: Vec<(String,)> = raw::fetch_all(c.exec(), &sql, &binds).await?;
        rows.into_iter()
            .map(|(text,)| serde_json::from_str(&text).map_err(MetaError::from))
            .collect()
    }

    /// Whether a row under `key` has the text `json`.
    async fn has_value<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<bool, Error> {
        let (filter, mut binds) = self.filter(owner, key);
        binds.push(json.into());
        let found: i64 = raw::fetch_scalar(
            c.exec(),
            &format!(
                "SELECT COUNT(*) FROM {} WHERE {filter} AND {} = ?",
                self.table, self.value
            ),
            &binds,
        )
        .await?;
        Ok(found > 0)
    }

    /// Deletes the rows under `key` whose text is `json`. True when there was one.
    async fn delete_value<C: Conn>(
        &self,
        c: &mut C,
        owner: i64,
        key: &str,
        json: &str,
    ) -> Result<bool, Error> {
        let (filter, mut binds) = self.filter(owner, key);
        binds.push(json.into());
        let deleted = raw::execute(
            c.exec(),
            &format!(
                "DELETE FROM {} WHERE {filter} AND {} = ?",
                self.table, self.value
            ),
            &binds,
        )
        .await?;
        Ok(deleted > 0)
    }

    /// Deletes every row under `key`. True when there was one.
    async fn delete<C: Conn>(&self, c: &mut C, owner: i64, key: &str) -> Result<bool, Error> {
        let (filter, binds) = self.filter(owner, key);
        let deleted = raw::execute(
            c.exec(),
            &format!("DELETE FROM {} WHERE {filter}", self.table),
            &binds,
        )
        .await?;
        Ok(deleted > 0)
    }
}

/// One of the meta tables, for code that serves posts, terms and users the same way (the REST
/// API's registered meta).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaTable {
    /// `post_meta`.
    Post,
    /// `term_meta`, keyed by the `terms` id.
    Term,
    /// `user_meta`.
    User,
}

impl MetaTable {
    fn store(self) -> &'static Store {
        match self {
            Self::Post => &POST_META,
            Self::Term => &TERM_META,
            Self::User => &USER_META,
        }
    }
}

/// How many owners one `IN (...)` of [`meta_rows`] binds.
const ROWS_CHUNK: usize = 500;

/// The rows of `table` whose owner is one of `owners` and whose key is one of `keys`, as
/// `(owner, key, text)` in the order they were added, at most `per_key` rows per owner and key
/// (the first ones by id): a key with more rows is never read whole. The key is compared exactly:
/// MySQL compares text without case, so rows whose key differs only in case are dropped here
/// (they count toward the same `per_key` rows there).
pub(crate) async fn meta_rows(
    db: &Database,
    table: MetaTable,
    owners: &[i64],
    keys: &[&str],
    per_key: usize,
) -> Result<Vec<(i64, String, String)>, Error> {
    let store = table.store();
    let owner = store.owner.expect("meta tables have an owner column");
    let mut out = Vec::new();
    if owners.is_empty() || keys.is_empty() || per_key == 0 {
        return Ok(out);
    }
    let key_marks = vec!["?"; keys.len()].join(", ");
    let per_key = i64::try_from(per_key).unwrap_or(i64::MAX);
    for chunk in owners.chunks(ROWS_CHUNK) {
        let mut binds: Vec<Sql> = chunk.iter().map(|id| Sql::from(*id)).collect();
        binds.extend(keys.iter().map(|k| Sql::from(*k)));
        binds.push(per_key.into());
        // `ROW_NUMBER()` numbers each owner's rows of a key, so the database returns at most
        // `per_key` of them; SQLite (3.25), MariaDB (10.2), MySQL (8.0) and PostgreSQL have it.
        let sql = format!(
            "SELECT m_owner, m_key, m_value FROM (SELECT {owner} AS m_owner, {key} AS m_key, \
             {value} AS m_value, id AS m_id, ROW_NUMBER() OVER (PARTITION BY {owner}, {key} \
             ORDER BY id) AS m_rank FROM {table} WHERE {owner} IN ({marks}) AND {key} IN \
             ({key_marks})) ranked WHERE m_rank <= ? ORDER BY m_id",
            key = store.key,
            value = store.value,
            table = store.table,
            marks = vec!["?"; chunk.len()].join(", "),
        );
        let rows: Vec<(i64, String, String)> = raw::fetch_all(db, &sql, &binds).await?;
        out.extend(
            rows.into_iter()
                .filter(|(_, key, _)| keys.contains(&key.as_str())),
        );
    }
    Ok(out)
}

/// Sets `key` of `owner` in `table` to `value`, replacing every row the key has, or inserting one,
/// as `update_metadata` does.
pub(crate) async fn set_meta(
    db: &Database,
    table: MetaTable,
    owner: i64,
    key: &str,
    value: &Value,
) -> Result<(), MetaError> {
    table.store().set(db, owner, key, value).await
}

/// Replaces the rows of `key` of `owner` in `table` with one row per value of `values`, in
/// order, in one transaction: the storage of a key registered with `single: false`.
pub(crate) async fn set_meta_values(
    db: &Database,
    table: MetaTable,
    owner: i64,
    key: &str,
    values: &[Value],
) -> Result<(), MetaError> {
    let store = table.store();
    db.transaction(async |tx| {
        store.delete(tx, owner, key).await?;
        for value in values {
            store.insert(tx, owner, key, &value.to_string()).await?;
        }
        Ok::<(), Error>(())
    })
    .await?;
    Ok(())
}

/// Deletes every row of `key` of `owner` in `table`. True when there was one.
pub(crate) async fn delete_meta(
    db: &Database,
    table: MetaTable,
    owner: i64,
    key: &str,
) -> Result<bool, MetaError> {
    Ok(table.store().delete(&mut &*db, owner, key).await?)
}

/// Post meta, set and read on a connection that is already in use (the steps of a transaction),
/// for the trash meta `Post::trash` keeps.
pub(crate) async fn put_post_meta_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
    value: &Value,
) -> Result<(), Error> {
    POST_META.put(c, post_id, key, &value.to_string()).await
}

/// Deletes post meta on a connection that is already in use.
pub(crate) async fn delete_post_meta_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
) -> Result<bool, Error> {
    POST_META.delete(c, post_id, key).await
}

/// Adds a value under post meta `key` on a connection that is already in use, keeping the values
/// already there, as `add_post_meta` does. For keys that hold a list, such as `_wp_old_slug`.
pub(crate) async fn add_post_meta_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
    value: &Value,
) -> Result<(), Error> {
    POST_META.insert(c, post_id, key, &value.to_string()).await
}

/// Deletes the values of post meta `key` equal to `value`, on a connection that is already in
/// use, as `delete_post_meta` with a value does.
pub(crate) async fn delete_post_meta_value_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
    value: &Value,
) -> Result<bool, Error> {
    POST_META
        .delete_value(c, post_id, key, &value.to_string())
        .await
}

/// Whether post meta `key` holds `value` among its values, on a connection that is already in use.
pub(crate) async fn post_meta_has_value_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
    value: &Value,
) -> Result<bool, Error> {
    POST_META
        .has_value(c, post_id, key, &value.to_string())
        .await
}

/// The value of post meta `key` on a connection that is already in use.
pub(crate) async fn get_post_meta_in<C: Conn>(
    c: &mut C,
    post_id: i64,
    key: &str,
) -> Result<Option<Value>, MetaError> {
    POST_META.get(c, post_id, key).await
}

/// Every value of post meta `key`, in the order they were added, as `get_post_meta` with
/// `$single = false` gives them. Empty when the post has none. Keys such as `_wp_old_slug` hold
/// one row per value.
///
/// # Errors
///
/// As [`get_post_meta`].
pub async fn get_post_meta_values(
    db: &Database,
    post_id: i64,
    key: &str,
) -> Result<Vec<Value>, MetaError> {
    POST_META.get_all(&mut &*db, post_id, key).await
}

/// The value of post meta `key`, or `None` when the post has none. A key that repeats gives its
/// first value.
///
/// # Errors
///
/// Fails with [`MetaError::InvalidJson`] when the stored text is not JSON, or with the database's
/// error.
pub async fn get_post_meta(
    db: &Database,
    post_id: i64,
    key: &str,
) -> Result<Option<Value>, MetaError> {
    POST_META.get(&mut &*db, post_id, key).await
}

/// Sets post meta `key` to `value`, replacing the stored value, or inserting it when the post has
/// none, as `update_post_meta` does.
///
/// # Errors
///
/// Fails with the database's error, such as a foreign key violation for a post that does not
/// exist.
pub async fn set_post_meta(
    db: &Database,
    post_id: i64,
    key: &str,
    value: &Value,
) -> Result<(), MetaError> {
    POST_META.set(db, post_id, key, value).await
}

/// Sets post meta `key` to `json`, text that must be valid JSON; stores it as given.
///
/// # Errors
///
/// Fails with [`MetaError::InvalidJson`] when `json` is not valid JSON; nothing is written. Or with
/// the database's error.
pub async fn set_post_meta_raw(
    db: &Database,
    post_id: i64,
    key: &str,
    json: &str,
) -> Result<(), MetaError> {
    POST_META.set_raw(db, post_id, key, json).await
}

/// Deletes post meta `key`. True when the post had it.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn delete_post_meta(db: &Database, post_id: i64, key: &str) -> Result<bool, MetaError> {
    Ok(POST_META.delete(&mut &*db, post_id, key).await?)
}

/// The value of user meta `key`, or `None`. See [`get_post_meta`].
///
/// # Errors
///
/// As [`get_post_meta`].
pub async fn get_user_meta(
    db: &Database,
    user_id: i64,
    key: &str,
) -> Result<Option<Value>, MetaError> {
    USER_META.get(&mut &*db, user_id, key).await
}

/// Sets user meta `key` to `value`. See [`set_post_meta`].
///
/// # Errors
///
/// As [`set_post_meta`].
pub async fn set_user_meta(
    db: &Database,
    user_id: i64,
    key: &str,
    value: &Value,
) -> Result<(), MetaError> {
    USER_META.set(db, user_id, key, value).await
}

/// Sets user meta `key` to `json`, text that must be valid JSON. See [`set_post_meta_raw`].
///
/// # Errors
///
/// As [`set_post_meta_raw`].
pub async fn set_user_meta_raw(
    db: &Database,
    user_id: i64,
    key: &str,
    json: &str,
) -> Result<(), MetaError> {
    USER_META.set_raw(db, user_id, key, json).await
}

/// Deletes user meta `key`. True when the user had it.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn delete_user_meta(db: &Database, user_id: i64, key: &str) -> Result<bool, MetaError> {
    Ok(USER_META.delete(&mut &*db, user_id, key).await?)
}

/// The value of term meta `key`, or `None`. `term_id` is the `terms` id, as in WordPress's
/// `get_term_meta`, not the `term_taxonomy` id. See [`get_post_meta`].
///
/// # Errors
///
/// As [`get_post_meta`].
pub async fn get_term_meta(
    db: &Database,
    term_id: i64,
    key: &str,
) -> Result<Option<Value>, MetaError> {
    TERM_META.get(&mut &*db, term_id, key).await
}

/// Sets term meta `key` to `value`. See [`set_post_meta`].
///
/// # Errors
///
/// As [`set_post_meta`].
pub async fn set_term_meta(
    db: &Database,
    term_id: i64,
    key: &str,
    value: &Value,
) -> Result<(), MetaError> {
    TERM_META.set(db, term_id, key, value).await
}

/// Sets term meta `key` to `json`, text that must be valid JSON. See [`set_post_meta_raw`].
///
/// # Errors
///
/// As [`set_post_meta_raw`].
pub async fn set_term_meta_raw(
    db: &Database,
    term_id: i64,
    key: &str,
    json: &str,
) -> Result<(), MetaError> {
    TERM_META.set_raw(db, term_id, key, json).await
}

/// Deletes term meta `key`. True when the term had it.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn delete_term_meta(db: &Database, term_id: i64, key: &str) -> Result<bool, MetaError> {
    Ok(TERM_META.delete(&mut &*db, term_id, key).await?)
}

/// The value of option `name`, or `None` when it is not set, as `get_option` without a default.
///
/// # Errors
///
/// As [`get_post_meta`].
pub async fn get_option(db: &Database, name: &str) -> Result<Option<Value>, MetaError> {
    OPTIONS.get(&mut &*db, 0, name).await
}

/// The site's offset from UTC: the `gmt_offset` option, in hours (a JSON number or numeric
/// text), as WordPress's date functions read it. An unset option, one that is not a number or not
/// JSON, and an offset of a day or more are UTC. The REST API, the front end and the admin's post
/// type screens all read it here.
///
/// # Errors
///
/// Fails with the database's error.
pub(crate) async fn gmt_offset(db: &Database) -> Result<chrono::FixedOffset, Error> {
    let hours = match get_option(db, "gmt_offset").await {
        Ok(Some(Value::Number(n))) => n.as_f64(),
        Ok(Some(Value::String(s))) => s.trim().parse().ok(),
        Ok(_) | Err(MetaError::InvalidJson(_)) => None,
        Err(MetaError::Database(e)) => return Err(e),
    };
    let utc = || chrono::FixedOffset::east_opt(0).expect("zero is a valid offset");
    Ok(
        chrono::FixedOffset::east_opt((hours.unwrap_or(0.0) * 3600.0).round() as i32)
            .unwrap_or_else(utc),
    )
}

/// Sets option `name` to `value`, replacing the stored value, as `update_option` does. A new
/// option gets the column's default `autoload`; a replaced one keeps its own.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn set_option(db: &Database, name: &str, value: &Value) -> Result<(), MetaError> {
    OPTIONS.set(db, 0, name, value).await
}

/// Sets option `name` to `json`, text that must be valid JSON. See [`set_post_meta_raw`].
///
/// # Errors
///
/// As [`set_post_meta_raw`].
pub async fn set_option_raw(db: &Database, name: &str, json: &str) -> Result<(), MetaError> {
    OPTIONS.set_raw(db, 0, name, json).await
}

/// Deletes option `name`. True when it was set.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn delete_option(db: &Database, name: &str) -> Result<bool, MetaError> {
    Ok(OPTIONS.delete(&mut &*db, 0, name).await?)
}
