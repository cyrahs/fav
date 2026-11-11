//! A grammers [`Session`] over Telethon's SQLite `.session` file, read and written in
//! Telethon's own format so the two stay interchangeable (DIVERGENCES.md D149).
//!
//! Python origin: Telethon 1.43's `sessions/sqlite.py` (format version 8). The file holds
//! one `sessions` row (the home DC, its address and its authorization key), the
//! `entities` cache (marked ids: users positive, chats negated, channels `-100…`; the row
//! `id = 0` holds the own user's id as its `hash`), and `update_state` (row 0 is the
//! account's `pts`/`qts`/`date`/`seq`, the others are channels by bare id). Telethon keeps
//! other DCs' keys in memory only, and so does this session. The file is created the way
//! Telethon creates it when it does not exist yet; an older version is upgraded as
//! Telethon upgrades it.

use std::fmt;
use std::net::{IpAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use grammers_client::session::types::{ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_client::session::{BoxFuture, Session, SessionData};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, Row as _};

/// `CURRENT_VERSION` of Telethon's session database.
pub const CURRENT_VERSION: i64 = 8;

/// The file Telethon opens for a session path: `.session` appended when missing.
pub fn session_file(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if text.ends_with(".session") {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("{text}.session"))
    }
}

/// A session file failure.
#[derive(Debug)]
pub struct SessionError(String);

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

impl From<sqlx::Error> for SessionError {
    fn from(error: sqlx::Error) -> Self {
        Self(format!("session database: {error}"))
    }
}

/// What the `sessions` row holds besides the key.
#[derive(Debug, Clone, Default)]
struct HomeRow {
    server_address: Option<String>,
    port: Option<i64>,
    takeout_id: Option<i64>,
}

#[derive(Default)]
struct State {
    data: SessionData,
    home: HomeRow,
    /// Whether the file names a home DC at all (Telethon's `dc_id` is 0 in a new file).
    home_known: bool,
    self_id: Option<i64>,
}

/// Telethon's marked id of a peer (`utils.get_peer_id`).
fn marked_id(peer: &PeerInfo) -> i64 {
    match peer {
        PeerInfo::User { id, .. } => *id,
        PeerInfo::Chat { id } => -*id,
        PeerInfo::Channel { id, .. } => -(1_000_000_000_000 + *id),
    }
}

/// A row of `entities` as grammers' peer, `None` for one it has no use for.
fn peer_from_row(id: i64, hash: i64) -> Option<PeerInfo> {
    if id > 0 {
        return Some(PeerInfo::User {
            id,
            auth: Some(PeerAuth::from_hash(hash)),
            bot: None,
            is_self: None,
        });
    }
    if (-999_999_999_999..=-1).contains(&id) {
        return Some(PeerInfo::Chat { id: -id });
    }
    let channel = -id - 1_000_000_000_000;
    (channel > 0).then(|| PeerInfo::Channel {
        id: channel,
        auth: Some(PeerAuth::from_hash(hash)),
        kind: None,
    })
}

fn now_float() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// The Telethon session of one account.
pub struct TelethonSession {
    path: PathBuf,
    pool: SqlitePool,
    state: Mutex<State>,
}

impl fmt::Debug for TelethonSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelethonSession").field("path", &self.path).finish_non_exhaustive()
    }
}

/// Telethon's `CREATE TABLE`s, in its order.
const TABLES: [&str; 5] = [
    "version (version integer primary key)",
    "sessions (\n                    dc_id integer primary key,\n                    server_address text,\n                    port integer,\n                    auth_key blob,\n                    takeout_id integer,\n                    tmp_auth_key blob\n                )",
    "entities (\n                    id integer primary key,\n                    hash integer not null,\n                    username text,\n                    phone integer,\n                    name text,\n                    date integer\n                )",
    "sent_files (\n                    md5_digest blob,\n                    file_size integer,\n                    type integer,\n                    id integer,\n                    hash integer,\n                    primary key(md5_digest, file_size, type)\n                )",
    "update_state (\n                    id integer primary key,\n                    pts integer,\n                    qts integer,\n                    date integer,\n                    seq integer\n                )",
];

impl TelethonSession {
    /// Open (or create) the session file Telethon would open for `path`.
    pub async fn open(path: &Path) -> Result<Self, SessionError> {
        let path = session_file(path);
        // Telethon's sqlite3 uses the rollback journal; WAL would change the file's mode.
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Delete)
            .foreign_keys(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .connect_with(options)
            .await?;
        let session = Self {
            path,
            pool,
            state: Mutex::new(State::default()),
        };
        session.load().await?;
        Ok(session)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn load(&self) -> Result<(), SessionError> {
        let has_version = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='version'")
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !has_version {
            for table in TABLES {
                sqlx::query(AssertSqlSafe(format!("CREATE TABLE {table}")))
                    .execute(&self.pool)
                    .await?;
            }
            sqlx::query("INSERT INTO version VALUES (?)")
                .bind(CURRENT_VERSION)
                .execute(&self.pool)
                .await?;
            self.write_home().await?;
            return Ok(());
        }
        let version: i64 = sqlx::query("SELECT version FROM version").fetch_one(&self.pool).await?.try_get(0)?;
        if version < CURRENT_VERSION {
            self.upgrade(version).await?;
            sqlx::query("DELETE FROM version").execute(&self.pool).await?;
            sqlx::query("INSERT INTO version VALUES (?)")
                .bind(CURRENT_VERSION)
                .execute(&self.pool)
                .await?;
        }
        let row = sqlx::query("SELECT dc_id, server_address, port, auth_key, takeout_id FROM sessions")
            .fetch_optional(&self.pool)
            .await?;
        let entities = sqlx::query("SELECT id, hash FROM entities").fetch_all(&self.pool).await?;
        let states = sqlx::query("SELECT id, pts, qts, date, seq FROM update_state")
            .fetch_all(&self.pool)
            .await?;
        let mut state = self.state();
        if let Some(row) = row {
            let dc_id: i64 = row.try_get(0)?;
            let server_address: Option<String> = row.try_get(1)?;
            let port: Option<i64> = row.try_get(2)?;
            let auth_key: Option<Vec<u8>> = row.try_get(3)?;
            state.home = HomeRow {
                server_address: server_address.clone(),
                port,
                // Telethon itself reads this column back into its temporary key and writes the
                // temporary key's column here, so it may hold `b''`: anything but a number is none.
                takeout_id: row.try_get::<Option<i64>, _>(4).ok().flatten(),
            };
            if let Ok(dc_id) = i32::try_from(dc_id)
                && dc_id > 0
            {
                state.home_known = true;
                state.data.home_dc = dc_id;
                let mut option = state.data.dc_options.get(&dc_id).cloned().unwrap_or(DcOption {
                    id: dc_id,
                    ipv4: SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 443),
                    ipv6: SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 443, 0, 0),
                    auth_key: None,
                });
                let port = port.and_then(|port| u16::try_from(port).ok()).unwrap_or(443);
                match server_address.as_deref().map(IpAddr::from_str) {
                    Some(Ok(IpAddr::V4(ip))) => option.ipv4 = SocketAddrV4::new(ip, port),
                    Some(Ok(IpAddr::V6(ip))) => option.ipv6 = SocketAddrV6::new(ip, port, 0, 0),
                    _ => {}
                }
                option.auth_key = auth_key.and_then(|key| <[u8; 256]>::try_from(key.as_slice()).ok());
                state.data.dc_options.insert(dc_id, option);
            }
        }
        for row in &entities {
            let id: i64 = row.try_get(0)?;
            let hash: i64 = row.try_get(1)?;
            if id == 0 {
                // Telethon's hack: the own user's id is saved as the hash of row 0.
                state.self_id = Some(hash);
                continue;
            }
            if let Some(peer) = peer_from_row(id, hash) {
                state.data.peer_infos.insert(peer.id(), peer);
            }
        }
        if let Some(self_id) = state.self_id.filter(|id| PeerId::user(*id).is_some())
            && let Some(PeerInfo::User { is_self, .. }) = state.data.peer_infos.get_mut(&PeerId::user_unchecked(self_id))
        {
            *is_self = Some(true);
        }
        let mut updates = UpdatesState::default();
        for row in &states {
            let id: i64 = row.try_get(0)?;
            let pts = int_column(row, 1);
            if id == 0 {
                updates.pts = pts;
                updates.qts = int_column(row, 2);
                updates.date = int_column(row, 3);
                updates.seq = int_column(row, 4);
            } else {
                updates.channels.push(ChannelState { id, pts });
            }
        }
        state.data.updates_state = updates;
        Ok(())
    }

    /// `_upgrade_database(old)`.
    async fn upgrade(&self, old: i64) -> Result<(), SessionError> {
        let mut old = old;
        if old == 1 {
            old += 1;
        }
        if old == 2 {
            old += 1;
            sqlx::query("DROP TABLE sent_files").execute(&self.pool).await?;
            sqlx::query(AssertSqlSafe(format!("CREATE TABLE {}", TABLES[3])))
                .execute(&self.pool)
                .await?;
        }
        if old == 3 {
            old += 1;
            sqlx::query(AssertSqlSafe(format!("CREATE TABLE {}", TABLES[4])))
                .execute(&self.pool)
                .await?;
        }
        if old == 4 {
            old += 1;
            sqlx::query("ALTER TABLE sessions ADD COLUMN takeout_id integer")
                .execute(&self.pool)
                .await?;
        }
        if old == 5 {
            old += 1;
            sqlx::query("DELETE FROM entities").execute(&self.pool).await?;
        }
        if old == 6 {
            old += 1;
            sqlx::query("ALTER TABLE entities ADD COLUMN date integer")
                .execute(&self.pool)
                .await?;
        }
        if old == 7 {
            sqlx::query("ALTER TABLE sessions ADD COLUMN tmp_auth_key blob")
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// `_update_session_table`: the one row of the home DC.
    async fn write_home(&self) -> Result<(), SessionError> {
        let (dc_id, address, port, key, takeout) = {
            let state = self.state();
            let dc_id = if state.home_known { state.data.home_dc } else { 0 };
            let option = state.data.dc_options.get(&dc_id);
            let address = option
                .map(|option| option.ipv4.ip().to_string())
                .or(state.home.server_address.clone());
            let port = option.map(|option| i64::from(option.ipv4.port())).or(state.home.port);
            let key = option
                .and_then(|option| option.auth_key)
                .map(|key| key.to_vec())
                .unwrap_or_default();
            (dc_id, address, port, key, state.home.takeout_id)
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM sessions").execute(&mut *transaction).await?;
        sqlx::query("INSERT OR REPLACE INTO sessions VALUES (?,?,?,?,?,?)")
            .bind(dc_id)
            .bind(address)
            .bind(port)
            .bind(key)
            .bind(takeout)
            .bind(Vec::<u8>::new())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn write_entity(&self, id: i64, hash: i64) -> Result<(), SessionError> {
        // Telethon writes username, phone and name too; grammers' peers do not carry them,
        // so an existing row keeps its own (D149).
        #[allow(clippy::cast_possible_truncation)] // reason: whole seconds since the epoch
        let date = now_float() as i64;
        sqlx::query(
            "INSERT INTO entities (id, hash, date) VALUES (?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET hash = excluded.hash, date = excluded.date",
        )
        .bind(id)
        .bind(hash)
        .bind(date)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn write_state(&self, id: i64, pts: i32, qts: i32, date: f64, seq: i32) -> Result<(), SessionError> {
        sqlx::query("INSERT OR REPLACE INTO update_state VALUES (?,?,?,?,?)")
            .bind(id)
            .bind(pts)
            .bind(qts)
            .bind(date)
            .bind(seq)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Close the file (Telethon's `session.close()`).
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn int_column(row: &sqlx::sqlite::SqliteRow, index: usize) -> i32 {
    // Telethon writes `date` as a float timestamp into an integer column.
    if let Ok(value) = row.try_get::<Option<i64>, _>(index) {
        return value.and_then(|value| i32::try_from(value).ok()).unwrap_or(0);
    }
    #[allow(clippy::cast_possible_truncation)] // reason: a timestamp in seconds fits i32 until 2038
    row.try_get::<Option<f64>, _>(index).ok().flatten().map_or(0, |value| value as i32)
}

impl Session for TelethonSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, SessionError> {
        Ok(self.state().data.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            {
                let mut state = self.state();
                state.data.home_dc = dc_id;
                state.home_known = true;
            }
            self.write_home().await
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SessionError> {
        Ok(self.state().data.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SessionError>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            let home = {
                let mut state = self.state();
                state.data.dc_options.insert(dc_option.id, dc_option.clone());
                state.home_known && state.data.home_dc == dc_option.id
            };
            // Only the home DC is on disk, as in Telethon.
            if home { self.write_home().await } else { Ok(()) }
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SessionError>> {
        Box::pin(async move {
            let state = self.state();
            if peer == PeerId::self_user() {
                return Ok(state
                    .self_id
                    .and_then(|id| state.data.peer_infos.get(&PeerId::user_unchecked(id)).cloned()));
            }
            Ok(state.data.peer_infos.get(&peer).cloned())
        })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let (write, new_self) = {
                let mut state = self.state();
                let before = state.data.peer_infos.get(&peer.id()).cloned();
                let entry = state.data.peer_infos.entry(peer.id()).or_insert_with(|| peer.clone());
                entry.extend_info(&peer);
                let after = entry.clone();
                let hash = after.auth().map(PeerAuth::hash);
                let changed = before.as_ref().and_then(PeerInfo::auth).map(PeerAuth::hash) != hash;
                let new_self = match &after {
                    PeerInfo::User {
                        id, is_self: Some(true), ..
                    } if state.self_id != Some(*id) => {
                        state.self_id = Some(*id);
                        Some(*id)
                    }
                    _ => None,
                };
                // Telethon skips peers it has no access hash for (`get_input_peer` fails).
                let write = match (&after, hash) {
                    (PeerInfo::Chat { .. }, _) if before.is_none() => Some((marked_id(&after), 0)),
                    (PeerInfo::User { .. } | PeerInfo::Channel { .. }, Some(hash)) if changed => Some((marked_id(&after), hash)),
                    _ => None,
                };
                (write, new_self)
            };
            if let Some((id, hash)) = write {
                self.write_entity(id, hash).await?;
            }
            if let Some(self_id) = new_self {
                self.write_entity(0, self_id).await?;
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SessionError>> {
        Box::pin(async move { Ok(self.state().data.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut writes: Vec<(i64, i32, i32, f64, i32)> = Vec::new();
            {
                let mut state = self.state();
                let updates = &mut state.data.updates_state;
                let primary = |updates: &UpdatesState| (0, updates.pts, updates.qts, f64::from(updates.date), updates.seq);
                match update {
                    UpdateState::All(all) => {
                        *updates = all;
                        writes.push(primary(updates));
                        let now = now_float();
                        writes.extend(updates.channels.iter().map(|channel| (channel.id, channel.pts, 0, now, 0)));
                    }
                    UpdateState::Primary { pts, date, seq } => {
                        updates.pts = pts;
                        updates.date = date;
                        updates.seq = seq;
                        writes.push(primary(updates));
                    }
                    UpdateState::Secondary { qts } => {
                        updates.qts = qts;
                        writes.push(primary(updates));
                    }
                    UpdateState::Channel { id, pts } => {
                        updates.channels.retain(|channel| channel.id != id);
                        updates.channels.push(ChannelState { id, pts });
                        // Telethon writes `datetime.now()` for a channel: any date works.
                        writes.push((id, pts, 0, now_float(), 0));
                    }
                }
            }
            for (id, pts, qts, date, seq) in writes {
                self.write_state(id, pts, qts, date, seq).await?;
            }
            Ok(())
        })
    }
}

/// The peers of a session, for tests.
#[cfg(test)]
pub(crate) fn peers(session: &TelethonSession) -> std::collections::HashMap<PeerId, PeerInfo> {
    session.state().data.peer_infos.clone()
}
