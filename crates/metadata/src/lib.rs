//! Product metadata in an embedded SQLite database.
//!
//! SQLite holds dashboards, alerts, API keys, sessions, settings and
//! aggregated query statistics. It never holds telemetry: the event stream
//! lives in the storage engine's WAL and segments.
//!
//! The API is synchronous; the server calls it from blocking tasks. One
//! connection behind a mutex is plenty for metadata-sized workloads.
//!
//! A database carries the layout identity ([`telemetry::LAYOUT`]): SQLite's
//! `application_id` is [`APPLICATION_ID`] and the one-row `vyrtel_layout`
//! table names the layout and [`SCHEMA_VERSION`]. A new database gets both
//! with its schema, in one transaction; any other database (another
//! layout, or one from before 0.1.2 without an identity) is refused before
//! it is written.

mod schema;

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    /// The database is not of this build's layout: another identity, or
    /// none. Opening it is refused before anything is written.
    #[error(
        "the metadata database has {found}, but this Vyrtel reads layout {} (schema version {SCHEMA_VERSION}) only",
        telemetry::LAYOUT
    )]
    ForeignLayout { found: String },
    /// Creating the schema of a new database failed. It is one
    /// transaction, so nothing of it was created; the next open retries.
    #[error("creating the metadata schema failed (nothing was created): {0}")]
    Create(rusqlite::Error),
}

/// The schema version of layout [`telemetry::LAYOUT`]: the only one.
pub const SCHEMA_VERSION: i64 = 1;

/// SQLite's `application_id` of a Vyrtel metadata database: "VYRT".
pub const APPLICATION_ID: i32 = 0x5659_5254;

/// What a metadata database is, judged by its identity alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// No schema yet (a new or empty file).
    Empty,
    /// Layout [`telemetry::LAYOUT`], schema [`SCHEMA_VERSION`].
    Current,
    /// Anything else, described for an error message.
    Foreign(String),
}

/// Identify the database on `conn`, reading only.
fn identify(conn: &Connection) -> Result<Identity> {
    let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0))?;
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if tables == 0 && app_id == 0 {
        return Ok(Identity::Empty);
    }
    let has = |name: &str| -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |r| r.get(0),
        )?)
    };
    if app_id != APPLICATION_ID || !has("vyrtel_layout")? {
        return Ok(Identity::Foreign(if has("schema_migrations")? {
            "no layout identity (it has schema_migrations: Vyrtel 0.1.1 or earlier, or Observer, wrote it)".into()
        } else {
            "no layout identity (it is not a Vyrtel metadata database)".into()
        }));
    }
    let row: Option<(String, i64)> = conn
        .query_row("SELECT layout, schema_version FROM vyrtel_layout WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(match row {
        None => Identity::Foreign("no layout identity (vyrtel_layout is empty)".into()),
        Some((layout, _)) if layout != telemetry::LAYOUT => Identity::Foreign(format!("layout {layout}")),
        Some((_, v)) if v != SCHEMA_VERSION => {
            Identity::Foreign(format!("layout {} schema version {v}", telemetry::LAYOUT))
        }
        Some(_) => Identity::Current,
    })
}

pub type Result<T> = std::result::Result<T, MetadataError>;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub struct Metadata {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub prefix: String,
    pub scope: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: i64,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    LogCount,
    MetricLine,
    SingleStat,
    SavedQuery,
}

impl PanelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PanelKind::LogCount => "log_count",
            PanelKind::MetricLine => "metric_line",
            PanelKind::SingleStat => "single_stat",
            PanelKind::SavedQuery => "saved_query",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "log_count" => Some(PanelKind::LogCount),
            "metric_line" => Some(PanelKind::MetricLine),
            "single_stat" => Some(PanelKind::SingleStat),
            "saved_query" => Some(PanelKind::SavedQuery),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Panel {
    pub id: i64,
    pub dashboard_id: i64,
    pub title: String,
    pub kind: PanelKind,
    /// Kind-specific settings (query, metric name, aggregation...).
    pub config: serde_json::Value,
    pub position: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelInput {
    pub title: String,
    pub kind: PanelKind,
    #[serde(default)]
    pub config: serde_json::Value,
    pub position: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub panels: Vec<Panel>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuery {
    pub id: i64,
    pub name: String,
    pub signal: String,
    pub query: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AlertStatus {
    Ok,
    Firing,
    Error,
}

impl AlertStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertStatus::Ok => "OK",
            AlertStatus::Firing => "FIRING",
            AlertStatus::Error => "ERROR",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OK" => Some(AlertStatus::Ok),
            "FIRING" => Some(AlertStatus::Firing),
            "ERROR" => Some(AlertStatus::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AlertInput {
    pub name: String,
    pub query: String,
    /// One of `>`, `>=`, `<`, `<=`, `=`, `!=`.
    pub op: String,
    pub threshold: f64,
    pub window_secs: i64,
    pub interval_secs: i64,
    pub webhook_url: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AlertState {
    pub status: AlertStatus,
    pub last_value: Option<f64>,
    pub last_evaluated_at: Option<i64>,
    pub last_transition_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_notification_at: Option<i64>,
    pub last_notification_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub id: i64,
    #[serde(flatten)]
    pub def: AlertInput,
    pub created_at: i64,
    pub updated_at: i64,
    pub state: AlertState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AlertTransition {
    pub from: String,
    pub to: String,
    pub value: Option<f64>,
    pub message: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StoredQueryStats {
    pub signal: String,
    pub field: String,
    pub index: String,
    pub queries: u64,
    pub bytes_read: u64,
    pub segments_scanned: u64,
    pub segments_skipped: u64,
    pub total_ms: f64,
    pub last_seen: i64,
}

pub const ALERT_OPS: [&str; 6] = [">", ">=", "<", "<=", "=", "!="];

impl AlertInput {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(MetadataError::Invalid("alert name is required".into()));
        }
        if !ALERT_OPS.contains(&self.op.as_str()) {
            return Err(MetadataError::Invalid(format!("condition must be one of {}", ALERT_OPS.join(" "))));
        }
        if !self.threshold.is_finite() {
            return Err(MetadataError::Invalid("threshold must be a finite number".into()));
        }
        if !(1..=31 * 86_400).contains(&self.window_secs) {
            return Err(MetadataError::Invalid("window must be between 1 second and 31 days".into()));
        }
        if !(10..=86_400).contains(&self.interval_secs) {
            return Err(MetadataError::Invalid("evaluation interval must be between 10 seconds and 1 day".into()));
        }
        if let Some(u) = &self.webhook_url
            && !(u.is_empty() || u.starts_with("http://") || u.starts_with("https://"))
        {
            return Err(MetadataError::Invalid("webhook URL must start with http:// or https://".into()));
        }
        Ok(())
    }
}

fn panel_from_row(r: &Row) -> rusqlite::Result<Panel> {
    let kind: String = r.get("kind")?;
    let config: String = r.get("config")?;
    Ok(Panel {
        id: r.get("id")?,
        dashboard_id: r.get("dashboard_id")?,
        title: r.get("title")?,
        kind: PanelKind::parse(&kind).unwrap_or(PanelKind::LogCount),
        config: serde_json::from_str(&config).unwrap_or(serde_json::Value::Null),
        position: r.get("position")?,
        width: r.get("width")?,
        height: r.get("height")?,
    })
}

fn alert_from_row(r: &Row) -> rusqlite::Result<Alert> {
    let status: Option<String> = r.get("state")?;
    Ok(Alert {
        id: r.get("id")?,
        def: AlertInput {
            name: r.get("name")?,
            query: r.get("query")?,
            op: r.get("condition_op")?,
            threshold: r.get("threshold")?,
            window_secs: r.get("window_secs")?,
            interval_secs: r.get("interval_secs")?,
            webhook_url: r.get("webhook_url")?,
            enabled: r.get::<_, i64>("enabled")? != 0,
        },
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        state: AlertState {
            status: status.as_deref().and_then(AlertStatus::parse).unwrap_or(AlertStatus::Ok),
            last_value: r.get("last_value")?,
            last_evaluated_at: r.get("last_evaluated_at")?,
            last_transition_at: r.get("last_transition_at")?,
            last_error: r.get("last_error")?,
            last_notification_at: r.get("last_notification_at")?,
            last_notification_error: r.get("last_notification_error")?,
        },
    })
}

/// An SQLite `file:` URI for `path` (forward slashes, a leading `/` before
/// a Windows drive letter, and `%`, `?` and `#` escaped).
fn sqlite_uri(path: &Path, immutable: bool) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file:");
    if p.as_bytes().get(1) == Some(&b':') {
        uri.push('/');
    }
    for c in p.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    if immutable {
        uri.push_str("?immutable=1");
    }
    uri
}

const ALERT_SELECT: &str = "SELECT a.*, s.state, s.last_value, s.last_evaluated_at, s.last_transition_at, \
     s.last_error, s.last_notification_at, s.last_notification_error \
     FROM alerts a LEFT JOIN alert_state s ON s.alert_id = a.id";

impl Metadata {
    /// Open the database, or create it with its schema if it has none.
    /// A database of another layout is refused unchanged.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| MetadataError::Invalid(format!("create metadata dir: {e}")))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// The identity of the database at `path`, read without changing
    /// anything: no journal or lock files are created when none exist.
    pub fn inspect(path: &Path) -> Result<Identity> {
        use rusqlite::OpenFlags;
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // `immutable` reads the file without locking or creating `-wal` and
        // `-shm` files; it would miss an unmerged `-wal`, so it is only used
        // when there is none (a cleanly closed database).
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let immutable = !Path::new(&wal).exists();
        let conn = Connection::open_with_flags(sqlite_uri(path, immutable), flags)?;
        identify(&conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        Self::init_with(conn, schema::SCHEMA)
    }

    fn init_with(mut conn: Connection, schema: &str) -> Result<Self> {
        // Before anything is written, even a journal mode.
        let identity = identify(&conn)?;
        if let Identity::Foreign(found) = identity {
            return Err(MetadataError::ForeignLayout { found });
        }
        // WAL for concurrent readers and crash safety; foreign keys are
        // off by default in SQLite and must be enabled per connection.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if identity == Identity::Empty {
            Self::create(&mut conn, schema).map_err(MetadataError::Create)?;
            tracing::info!(layout = telemetry::LAYOUT, schema = SCHEMA_VERSION, "created metadata database");
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// The schema and the identity, in one transaction: an interruption (a
    /// failed statement, a crash, a full disk) leaves the database empty,
    /// and the next open creates it again.
    fn create(conn: &mut Connection, schema: &str) -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        tx.execute_batch(schema)?;
        tx.execute(
            "INSERT INTO vyrtel_layout (id, layout, schema_version, created_at) VALUES (1, ?1, ?2, ?3)",
            params![telemetry::LAYOUT, SCHEMA_VERSION, now_ms()],
        )?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.commit()
    }

    pub fn schema_version(&self) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT schema_version FROM vyrtel_layout WHERE id = 1", [], |r| r.get(0))?)
    }

    // ---- settings -------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(c.prepare_cached("SELECT value FROM settings WHERE key = ?1")?.query_row([key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.prepare_cached(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )?
        .execute(params![key, value, now_ms()])?;
        Ok(())
    }

    // ---- API keys -------------------------------------------------------

    /// Store a new key. Only the hash is persisted; the caller shows the
    /// plaintext to the user once.
    pub fn create_api_key(&self, name: &str, prefix: &str, key_hash: &str, scope: &str) -> Result<ApiKey> {
        if name.trim().is_empty() {
            return Err(MetadataError::Invalid("key name is required".into()));
        }
        if scope != "ingest" && scope != "admin" {
            return Err(MetadataError::Invalid("scope must be 'ingest' or 'admin'".into()));
        }
        let c = self.conn.lock().unwrap();
        let now = now_ms();
        c.prepare_cached(
            "INSERT INTO api_keys (name, prefix, key_hash, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![name.trim(), prefix, key_hash, scope, now])?;
        Ok(ApiKey {
            id: c.last_insert_rowid(),
            name: name.trim().to_string(),
            prefix: prefix.to_string(),
            scope: scope.to_string(),
            created_at: now,
            last_used_at: None,
        })
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached(
            "SELECT id, name, prefix, scope, created_at, last_used_at FROM api_keys WHERE revoked_at IS NULL ORDER BY id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ApiKey {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                scope: r.get(3)?,
                created_at: r.get(4)?,
                last_used_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every active key as `(id, hash, scope)`.
    pub fn active_api_keys(&self) -> Result<Vec<(i64, String, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached("SELECT id, key_hash, scope FROM api_keys WHERE revoked_at IS NULL")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn touch_api_key(&self, id: i64) -> Result<()> {
        self.touch_api_keys(&[id]).map(|_| ())
    }

    /// Set `last_used_at` to now for every active key in `ids`, in one
    /// transaction. Revoked keys are left as they are. Returns how many
    /// keys were updated.
    pub fn touch_api_keys(&self, ids: &[i64]) -> Result<usize> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let now = now_ms();
        let mut n = 0;
        {
            let mut st =
                tx.prepare_cached("UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1 AND revoked_at IS NULL")?;
            for id in ids {
                n += st.execute(params![id, now])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn revoke_api_key(&self, id: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let n = c
            .prepare_cached("UPDATE api_keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL")?
            .execute(params![id, now_ms()])?;
        if n == 0 {
            return Err(MetadataError::NotFound);
        }
        Ok(())
    }

    pub fn count_api_keys(&self) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT COUNT(*) FROM api_keys WHERE revoked_at IS NULL", [], |r| r.get(0))?)
    }

    // ---- users & sessions ---------------------------------------------

    pub fn upsert_user(&self, username: &str, password_hash: &str) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        c.prepare_cached(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(username) DO UPDATE SET password_hash = excluded.password_hash",
        )?
        .execute(params![username, password_hash, now_ms()])?;
        Ok(c.query_row("SELECT id FROM users WHERE username = ?1", [username], |r| r.get(0))?)
    }

    /// Set the password hash of `username` (creating the user if needed)
    /// and end all of that user's sessions, in one transaction, so no
    /// session issued under the old password outlives the change. Returns
    /// the user id and how many sessions were ended.
    pub fn rotate_password(&self, username: &str, password_hash: &str) -> Result<(i64, usize)> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.prepare_cached(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(username) DO UPDATE SET password_hash = excluded.password_hash",
        )?
        .execute(params![username, password_hash, now_ms()])?;
        let id: i64 = tx.query_row("SELECT id FROM users WHERE username = ?1", [username], |r| r.get(0))?;
        let ended = tx.prepare_cached("DELETE FROM sessions WHERE user_id = ?1")?.execute([id])?;
        tx.commit()?;
        Ok((id, ended))
    }

    pub fn find_user(&self, username: &str) -> Result<Option<User>> {
        let c = self.conn.lock().unwrap();
        Ok(c.prepare_cached("SELECT id, username, password_hash FROM users WHERE username = ?1")?
            .query_row([username], |r| Ok(User { id: r.get(0)?, username: r.get(1)?, password_hash: r.get(2)? }))
            .optional()?)
    }

    pub fn count_users(&self) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    pub fn create_session(&self, token_hash: &str, user_id: i64, ttl_ms: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let now = now_ms();
        c.prepare_cached("INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![token_hash, user_id, now, now + ttl_ms])?;
        Ok(())
    }

    pub fn find_session(&self, token_hash: &str) -> Result<Option<User>> {
        self.find_session_timed(token_hash).map(|(found, _)| found.map(|(user, _)| user))
    }

    /// [`Metadata::find_session`] with the session's expiry (ms since the
    /// epoch), also returning how long it waited for the connection, so
    /// request authentication can tell waiting behind other metadata work
    /// from the lookup itself.
    #[allow(clippy::type_complexity)]
    pub fn find_session_timed(&self, token_hash: &str) -> Result<(Option<(User, i64)>, std::time::Duration)> {
        let waiting = std::time::Instant::now();
        let c = self.conn.lock().unwrap();
        let lock_wait = waiting.elapsed();
        let found = c
            .prepare_cached(
                "SELECT u.id, u.username, u.password_hash, s.expires_at FROM sessions s \
                 JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?1 AND s.expires_at > ?2",
            )?
            .query_row(params![token_hash, now_ms()], |r| {
                Ok((User { id: r.get(0)?, username: r.get(1)?, password_hash: r.get(2)? }, r.get(3)?))
            })
            .optional()?;
        Ok((found, lock_wait))
    }

    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.prepare_cached("DELETE FROM sessions WHERE token_hash = ?1")?.execute([token_hash])?;
        Ok(())
    }

    pub fn purge_expired_sessions(&self) -> Result<usize> {
        let c = self.conn.lock().unwrap();
        Ok(c.prepare_cached("DELETE FROM sessions WHERE expires_at <= ?1")?.execute([now_ms()])?)
    }

    // ---- dashboards -----------------------------------------------------

    pub fn create_dashboard(&self, name: &str, description: Option<&str>) -> Result<Dashboard> {
        validate_name(name, "dashboard")?;
        let c = self.conn.lock().unwrap();
        let now = now_ms();
        c.prepare_cached("INSERT INTO dashboards (name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)")?
            .execute(params![name.trim(), description, now])?;
        let id = c.last_insert_rowid();
        drop(c);
        self.get_dashboard(id)
    }

    pub fn list_dashboards(&self) -> Result<Vec<Dashboard>> {
        let ids: Vec<i64> = {
            let c = self.conn.lock().unwrap();
            let mut st = c.prepare_cached("SELECT id FROM dashboards ORDER BY name COLLATE NOCASE, id")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter().map(|id| self.get_dashboard(id)).collect()
    }

    pub fn get_dashboard(&self, id: i64) -> Result<Dashboard> {
        let c = self.conn.lock().unwrap();
        let mut d = c
            .prepare_cached("SELECT id, name, description, created_at, updated_at FROM dashboards WHERE id = ?1")?
            .query_row([id], |r| {
                Ok(Dashboard {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                    panels: Vec::new(),
                })
            })
            .optional()?
            .ok_or(MetadataError::NotFound)?;
        let mut st =
            c.prepare_cached("SELECT * FROM dashboard_panels WHERE dashboard_id = ?1 ORDER BY position, id")?;
        d.panels = st.query_map([id], panel_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(d)
    }

    pub fn update_dashboard(&self, id: i64, name: &str, description: Option<&str>) -> Result<Dashboard> {
        validate_name(name, "dashboard")?;
        {
            let c = self.conn.lock().unwrap();
            let n = c
                .prepare_cached("UPDATE dashboards SET name = ?2, description = ?3, updated_at = ?4 WHERE id = ?1")?
                .execute(params![id, name.trim(), description, now_ms()])?;
            if n == 0 {
                return Err(MetadataError::NotFound);
            }
        }
        self.get_dashboard(id)
    }

    pub fn delete_dashboard(&self, id: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        // Panels go with it (ON DELETE CASCADE).
        if c.prepare_cached("DELETE FROM dashboards WHERE id = ?1")?.execute([id])? == 0 {
            return Err(MetadataError::NotFound);
        }
        Ok(())
    }

    pub fn add_panel(&self, dashboard_id: i64, p: &PanelInput) -> Result<Panel> {
        validate_name(&p.title, "panel")?;
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let exists: bool = tx
            .query_row("SELECT 1 FROM dashboards WHERE id = ?1", [dashboard_id], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(MetadataError::NotFound);
        }
        let position = match p.position {
            Some(pos) => pos,
            None => tx.query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM dashboard_panels WHERE dashboard_id = ?1",
                [dashboard_id],
                |r| r.get(0),
            )?,
        };
        let now = now_ms();
        tx.execute(
            "INSERT INTO dashboard_panels (dashboard_id, title, kind, config, position, width, height, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                dashboard_id,
                p.title.trim(),
                p.kind.as_str(),
                p.config.to_string(),
                position,
                p.width.unwrap_or(6).clamp(1, 12),
                p.height.unwrap_or(4).clamp(1, 12),
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("UPDATE dashboards SET updated_at = ?2 WHERE id = ?1", params![dashboard_id, now])?;
        let panel = tx.query_row("SELECT * FROM dashboard_panels WHERE id = ?1", [id], panel_from_row)?;
        tx.commit()?;
        Ok(panel)
    }

    pub fn update_panel(&self, dashboard_id: i64, panel_id: i64, p: &PanelInput) -> Result<Panel> {
        validate_name(&p.title, "panel")?;
        let c = self.conn.lock().unwrap();
        let now = now_ms();
        let n = c
            .prepare_cached(
                "UPDATE dashboard_panels SET title = ?3, kind = ?4, config = ?5, \
                 position = COALESCE(?6, position), width = COALESCE(?7, width), height = COALESCE(?8, height), updated_at = ?9 \
                 WHERE id = ?1 AND dashboard_id = ?2",
            )?
            .execute(params![
                panel_id,
                dashboard_id,
                p.title.trim(),
                p.kind.as_str(),
                p.config.to_string(),
                p.position,
                p.width.map(|w| w.clamp(1, 12)),
                p.height.map(|h| h.clamp(1, 12)),
                now
            ])?;
        if n == 0 {
            return Err(MetadataError::NotFound);
        }
        c.execute("UPDATE dashboards SET updated_at = ?2 WHERE id = ?1", params![dashboard_id, now])?;
        Ok(c.query_row("SELECT * FROM dashboard_panels WHERE id = ?1", [panel_id], panel_from_row)?)
    }

    pub fn delete_panel(&self, dashboard_id: i64, panel_id: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let n = c
            .prepare_cached("DELETE FROM dashboard_panels WHERE id = ?1 AND dashboard_id = ?2")?
            .execute(params![panel_id, dashboard_id])?;
        if n == 0 {
            return Err(MetadataError::NotFound);
        }
        Ok(())
    }

    // ---- saved queries --------------------------------------------------

    pub fn create_saved_query(&self, name: &str, signal: &str, query: &str) -> Result<SavedQuery> {
        validate_name(name, "saved query")?;
        if !matches!(signal, "logs" | "traces" | "metrics") {
            return Err(MetadataError::Invalid("signal must be logs, traces or metrics".into()));
        }
        let c = self.conn.lock().unwrap();
        let now = now_ms();
        c.prepare_cached("INSERT INTO saved_queries (name, signal, query, created_at) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![name.trim(), signal, query, now])?;
        Ok(SavedQuery {
            id: c.last_insert_rowid(),
            name: name.trim().into(),
            signal: signal.into(),
            query: query.into(),
            created_at: now,
        })
    }

    pub fn list_saved_queries(&self) -> Result<Vec<SavedQuery>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached(
            "SELECT id, name, signal, query, created_at FROM saved_queries ORDER BY name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SavedQuery {
                id: r.get(0)?,
                name: r.get(1)?,
                signal: r.get(2)?,
                query: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_saved_query(&self, id: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        if c.prepare_cached("DELETE FROM saved_queries WHERE id = ?1")?.execute([id])? == 0 {
            return Err(MetadataError::NotFound);
        }
        Ok(())
    }

    // ---- alerts ---------------------------------------------------------

    pub fn create_alert(&self, a: &AlertInput) -> Result<Alert> {
        a.validate()?;
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO alerts (name, query, condition_op, threshold, window_secs, interval_secs, webhook_url, enabled, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                a.name.trim(),
                a.query,
                a.op,
                a.threshold,
                a.window_secs,
                a.interval_secs,
                a.webhook_url.as_deref().filter(|u| !u.is_empty()),
                a.enabled as i64,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO alert_state (alert_id, state) VALUES (?1, 'OK')", [id])?;
        tx.commit()?;
        drop(c);
        self.get_alert(id)
    }

    pub fn update_alert(&self, id: i64, a: &AlertInput) -> Result<Alert> {
        a.validate()?;
        {
            let c = self.conn.lock().unwrap();
            let n = c
                .prepare_cached(
                    "UPDATE alerts SET name = ?2, query = ?3, condition_op = ?4, threshold = ?5, window_secs = ?6, \
                     interval_secs = ?7, webhook_url = ?8, enabled = ?9, updated_at = ?10 WHERE id = ?1",
                )?
                .execute(params![
                    id,
                    a.name.trim(),
                    a.query,
                    a.op,
                    a.threshold,
                    a.window_secs,
                    a.interval_secs,
                    a.webhook_url.as_deref().filter(|u| !u.is_empty()),
                    a.enabled as i64,
                    now_ms()
                ])?;
            if n == 0 {
                return Err(MetadataError::NotFound);
            }
        }
        self.get_alert(id)
    }

    pub fn get_alert(&self, id: i64) -> Result<Alert> {
        let c = self.conn.lock().unwrap();
        c.prepare_cached(&format!("{ALERT_SELECT} WHERE a.id = ?1"))?
            .query_row([id], alert_from_row)
            .optional()?
            .ok_or(MetadataError::NotFound)
    }

    pub fn list_alerts(&self) -> Result<Vec<Alert>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached(&format!("{ALERT_SELECT} ORDER BY a.name COLLATE NOCASE, a.id"))?;
        let rows = st.query_map([], alert_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_alert(&self, id: i64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        if c.prepare_cached("DELETE FROM alerts WHERE id = ?1")?.execute([id])? == 0 {
            return Err(MetadataError::NotFound);
        }
        Ok(())
    }

    /// Persist an evaluation result; records a transition when the status
    /// changed. Returns the previous status.
    pub fn record_alert_evaluation(
        &self,
        id: i64,
        status: AlertStatus,
        value: Option<f64>,
        error: Option<&str>,
    ) -> Result<AlertStatus> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let prev: String = tx
            .query_row("SELECT state FROM alert_state WHERE alert_id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or(MetadataError::NotFound)?;
        let prev = AlertStatus::parse(&prev).unwrap_or(AlertStatus::Ok);
        let now = now_ms();
        if prev != status {
            tx.execute(
                "UPDATE alert_state SET state = ?2, last_value = ?3, last_evaluated_at = ?4, last_transition_at = ?4, last_error = ?5 WHERE alert_id = ?1",
                params![id, status.as_str(), value, now, error],
            )?;
            tx.execute(
                "INSERT INTO alert_history (alert_id, from_state, to_state, value, message, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, prev.as_str(), status.as_str(), value, error, now],
            )?;
        } else {
            tx.execute(
                "UPDATE alert_state SET last_value = ?2, last_evaluated_at = ?3, last_error = ?4 WHERE alert_id = ?1",
                params![id, value, now, error],
            )?;
        }
        tx.commit()?;
        Ok(prev)
    }

    pub fn record_alert_notification(&self, id: i64, error: Option<&str>) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.prepare_cached(
            "UPDATE alert_state SET last_notification_at = ?2, last_notification_error = ?3 WHERE alert_id = ?1",
        )?
        .execute(params![id, now_ms(), error])?;
        Ok(())
    }

    pub fn alert_history(&self, id: i64, limit: usize) -> Result<Vec<AlertTransition>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached(
            "SELECT from_state, to_state, value, message, at FROM alert_history WHERE alert_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![id, limit as i64], |r| {
            Ok(AlertTransition { from: r.get(0)?, to: r.get(1)?, value: r.get(2)?, message: r.get(3)?, at: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- query statistics ----------------------------------------------

    /// Add a batch of increments to the persisted totals.
    pub fn add_query_stats(&self, rows: &[StoredQueryStats]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO query_stats (signal, field, index_kind, queries, bytes_read, segments_scanned, segments_skipped, total_ms, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(signal, field) DO UPDATE SET \
                   index_kind = excluded.index_kind, \
                   queries = queries + excluded.queries, \
                   bytes_read = bytes_read + excluded.bytes_read, \
                   segments_scanned = segments_scanned + excluded.segments_scanned, \
                   segments_skipped = segments_skipped + excluded.segments_skipped, \
                   total_ms = total_ms + excluded.total_ms, \
                   last_seen = excluded.last_seen",
            )?;
            for r in rows {
                st.execute(params![
                    r.signal,
                    r.field,
                    r.index,
                    r.queries as i64,
                    r.bytes_read as i64,
                    r.segments_scanned as i64,
                    r.segments_skipped as i64,
                    r.total_ms,
                    r.last_seen
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn query_stats(&self) -> Result<Vec<StoredQueryStats>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare_cached(
            "SELECT signal, field, index_kind, queries, bytes_read, segments_scanned, segments_skipped, total_ms, last_seen \
             FROM query_stats ORDER BY queries DESC, field",
        )?;
        let rows = st.query_map([], |r| {
            Ok(StoredQueryStats {
                signal: r.get(0)?,
                field: r.get(1)?,
                index: r.get(2)?,
                queries: r.get::<_, i64>(3)? as u64,
                bytes_read: r.get::<_, i64>(4)? as u64,
                segments_scanned: r.get::<_, i64>(5)? as u64,
                segments_skipped: r.get::<_, i64>(6)? as u64,
                total_ms: r.get(7)?,
                last_seen: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn validate_name(name: &str, what: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() {
        return Err(MetadataError::Invalid(format!("{what} name is required")));
    }
    if n.chars().count() > 200 {
        return Err(MetadataError::Invalid(format!("{what} name is too long")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(path: &Path) -> Vec<String> {
        let c = Connection::open(path).unwrap();
        let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    /// The schema, then a statement that fails: creation stops part-way.
    fn failing_schema() -> String {
        format!("{}\nINSERT INTO missing VALUES (1);", schema::SCHEMA)
    }

    #[test]
    fn an_interrupted_creation_leaves_nothing_and_is_redone() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("metadata.db");
        match Metadata::init_with(Connection::open(&path).unwrap(), &failing_schema()) {
            Err(e @ MetadataError::Create(_)) => {
                assert!(e.to_string().starts_with("creating the metadata schema failed (nothing was created): "))
            }
            Err(e) => panic!("expected Create, got {e}"),
            Ok(_) => panic!("creation must fail"),
        }
        assert_eq!(Metadata::inspect(&path).unwrap(), Identity::Empty);
        assert!(tables(&path).is_empty(), "nothing of the schema remains");

        let m = Metadata::open(&path).unwrap();
        assert_eq!(m.schema_version().unwrap(), SCHEMA_VERSION);
        drop(m);
        assert_eq!(Metadata::inspect(&path).unwrap(), Identity::Current);
        let c = Connection::open(&path).unwrap();
        let app_id: i32 = c.query_row("PRAGMA application_id", [], |r| r.get(0)).unwrap();
        assert_eq!(app_id, APPLICATION_ID);
        let layout: String = c.query_row("SELECT layout FROM vyrtel_layout", [], |r| r.get(0)).unwrap();
        assert_eq!(layout, telemetry::LAYOUT);
    }
}
