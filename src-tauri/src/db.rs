use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ────────────────────────────────────────────────────────────────────
// Schema
// ────────────────────────────────────────────────────────────────────

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS backup_history (
    id              TEXT PRIMARY KEY,
    filename        TEXT NOT NULL,
    remote_key      TEXT NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    encrypted_size  INTEGER NOT NULL DEFAULT 0,
    started_at      TEXT NOT NULL,
    completed_at    TEXT,
    status          TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS backup_profiles (
    id          TEXT PRIMARY KEY,
    owner_account TEXT,
    name        TEXT NOT NULL,
    source_path TEXT NOT NULL,
    schedule    TEXT,
    retention   INTEGER DEFAULT 0,
    folder      TEXT NOT NULL DEFAULT '/',
    enabled     INTEGER DEFAULT 1,
    last_run    TEXT,
    next_run    TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    retry_at    TEXT,
    last_error  TEXT,
    last_error_code TEXT,
    schedule_state TEXT NOT NULL DEFAULT 'scheduled',
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS file_snapshots (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id  TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    file_hash   TEXT NOT NULL,
    file_size   INTEGER NOT NULL,
    modified_at TEXT NOT NULL,
    backup_id   TEXT NOT NULL,
    UNIQUE(profile_id, file_path)
);

CREATE TABLE IF NOT EXISTS app_metadata (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Safely add the profile_id column if it doesn't exist yet.
const MIGRATE_PROFILE_ID: &str = r#"
ALTER TABLE backup_history ADD COLUMN profile_id TEXT;
"#;

const MIGRATE_PROFILE_FOLDER: &str = r#"
ALTER TABLE backup_profiles ADD COLUMN folder TEXT NOT NULL DEFAULT '/';
"#;

const MIGRATE_PROFILE_OWNER: &str = r#"
ALTER TABLE backup_profiles ADD COLUMN owner_account TEXT;
"#;

const PROFILE_RESILIENCE_MIGRATIONS: &[&str] = &[
    "ALTER TABLE backup_profiles ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE backup_profiles ADD COLUMN retry_at TEXT",
    "ALTER TABLE backup_profiles ADD COLUMN last_error TEXT",
    "ALTER TABLE backup_profiles ADD COLUMN last_error_code TEXT",
    "ALTER TABLE backup_profiles ADD COLUMN schedule_state TEXT NOT NULL DEFAULT 'scheduled'",
];

// ────────────────────────────────────────────────────────────────────
// Row types
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    pub filename: String,
    pub remote_key: String,
    pub size: i64,
    pub encrypted_size: i64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupProfile {
    pub id: String,
    #[serde(default, skip_serializing)]
    pub owner_account: Option<String>,
    pub name: String,
    pub source_path: String,
    pub schedule: Option<String>,
    pub retention: i64,
    pub folder: String,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub retry_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_code: Option<String>,
    #[serde(default = "default_schedule_state")]
    pub schedule_state: String,
    pub created_at: String,
}

fn default_schedule_state() -> String {
    "scheduled".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub file_path: String,
    pub file_hash: String,
    pub file_size: i64,
    pub modified_at: String,
    pub backup_id: String,
}

// ────────────────────────────────────────────────────────────────────
// Initialization
// ────────────────────────────────────────────────────────────────────

/// Open (or create) the SQLite database and run the schema migration.
pub fn init_db(data_dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(data_dir).context("Failed to create data directory")?;

    let db_path = data_dir.join("savestate.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open DB at {:?}", db_path))?;

    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .context("Failed to set PRAGMA")?;

    conn.execute_batch(SCHEMA)
        .context("Failed to run DB schema")?;

    // Safe migration: add profile_id column to backup_history if missing
    let _ = conn.execute_batch(MIGRATE_PROFILE_ID);
    // Safe migration: existing profiles continue writing snapshots to root.
    let _ = conn.execute_batch(MIGRATE_PROFILE_FOLDER);
    // Legacy profiles remain deliberately unclaimed until a signed-in user
    // explicitly assigns them. This prevents an account switch during upgrade
    // from silently transferring another account's schedules.
    migrate_profile_ownership(&conn)?;
    // Safe migrations: persist retry/backoff state across restarts. Duplicate
    // column errors are expected once each migration has already been applied.
    for migration in PROFILE_RESILIENCE_MIGRATIONS {
        let _ = conn.execute(migration, []);
    }
    normalize_disabled_profile_state(&conn)?;

    Ok(conn)
}

fn migrate_profile_ownership(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch(MIGRATE_PROFILE_OWNER);
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_backup_profiles_owner_account
         ON backup_profiles(owner_account)",
        [],
    )
    .context("Failed to index profile ownership")?;
    Ok(())
}

fn normalize_disabled_profile_state(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE backup_profiles
         SET next_run = NULL, retry_count = 0, retry_at = NULL,
             last_error = NULL, last_error_code = NULL, schedule_state = 'disabled'
         WHERE enabled = 0",
        [],
    )
    .context("Failed to normalize disabled profile schedule state")?;
    Ok(())
}

/// Return a stable, pseudonymous identifier for this app installation.
/// It is safe to send with operational telemetry and contains no hostname,
/// username, email address, path, or hardware identifier.
pub fn get_or_create_installation_id(conn: &Connection) -> Result<String> {
    if let Ok(value) = conn.query_row(
        "SELECT value FROM app_metadata WHERE key = 'installation_id'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        return Ok(value);
    }

    let value = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT OR IGNORE INTO app_metadata (key, value) VALUES ('installation_id', ?1)",
        params![value],
    )?;
    conn.query_row(
        "SELECT value FROM app_metadata WHERE key = 'installation_id'",
        [],
        |row| row.get(0),
    )
    .context("Failed to persist installation ID")
}

pub fn get_app_metadata(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT value FROM app_metadata WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn set_app_metadata(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_metadata (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Backup history functions
// ────────────────────────────────────────────────────────────────────

/// Insert a new backup record (status = "in_progress").
pub fn record_backup_start(
    conn: &Connection,
    id: &str,
    filename: &str,
    remote_key: &str,
    size: i64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO backup_history (id, filename, remote_key, size, encrypted_size, started_at, status)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, 'in_progress')",
        params![id, filename, remote_key, size, now],
    )
    .context("Failed to insert backup record")?;
    Ok(())
}

/// Mark a backup as completed with the final encrypted size.
pub fn record_backup_complete(conn: &Connection, id: &str, encrypted_size: i64) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE backup_history SET status = 'completed', encrypted_size = ?1, completed_at = ?2 WHERE id = ?3",
        params![encrypted_size, now, id],
    )
    .context("Failed to update backup record")?;
    Ok(())
}

/// Mark a backup as failed.
pub fn record_backup_failed(conn: &Connection, id: &str, error_msg: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE backup_history SET status = ?1, completed_at = ?2 WHERE id = ?3",
        params![format!("failed: {}", error_msg), now, id],
    )
    .context("Failed to update backup record")?;
    Ok(())
}

/// Return all backup records, newest first.
pub fn get_backup_history(conn: &Connection) -> Result<Vec<BackupRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, filename, remote_key, size, encrypted_size, started_at, completed_at, status
             FROM backup_history
             ORDER BY started_at DESC",
        )
        .context("Failed to prepare history query")?;

    let rows = stmt
        .query_map([], |row| {
            Ok(BackupRecord {
                id: row.get(0)?,
                filename: row.get(1)?,
                remote_key: row.get(2)?,
                size: row.get(3)?,
                encrypted_size: row.get(4)?,
                started_at: row.get(5)?,
                completed_at: row.get(6)?,
                status: row.get(7)?,
            })
        })
        .context("Failed to execute history query")?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row.context("Failed to read row")?);
    }
    Ok(records)
}

/// Delete a local backup record by id.
pub fn delete_backup_record(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM backup_history WHERE id = ?1", params![id])
        .context("Failed to delete backup record")?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Profile CRUD
// ────────────────────────────────────────────────────────────────────

/// Create a new backup profile.
pub fn create_profile(conn: &Connection, profile: &BackupProfile) -> Result<()> {
    conn.execute(
        "INSERT INTO backup_profiles
           (id, owner_account, name, source_path, schedule, retention, folder, enabled, last_run,
            next_run, retry_count, retry_at, last_error, last_error_code, schedule_state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            profile.id,
            profile.owner_account,
            profile.name,
            profile.source_path,
            profile.schedule,
            profile.retention,
            profile.folder,
            profile.enabled as i32,
            profile.last_run,
            profile.next_run,
            profile.retry_count,
            profile.retry_at,
            profile.last_error,
            profile.last_error_code,
            profile.schedule_state,
            profile.created_at,
        ],
    )
    .context("Failed to create backup profile")?;
    Ok(())
}

/// Update an existing backup profile.
pub fn update_profile(conn: &Connection, profile: &BackupProfile) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles
         SET name = ?1, source_path = ?2, schedule = ?3, retention = ?4, folder = ?5,
             enabled = ?6, last_run = ?7, next_run = ?8, retry_count = ?9,
             retry_at = ?10, last_error = ?11, last_error_code = ?12, schedule_state = ?13
         WHERE id = ?14 AND owner_account IS ?15",
            params![
                profile.name,
                profile.source_path,
                profile.schedule,
                profile.retention,
                profile.folder,
                profile.enabled as i32,
                profile.last_run,
                profile.next_run,
                profile.retry_count,
                profile.retry_at,
                profile.last_error,
                profile.last_error_code,
                profile.schedule_state,
                profile.id,
                profile.owner_account,
            ],
        )
        .context("Failed to update backup profile")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    Ok(())
}

/// Delete a backup profile only when it belongs to the signed-in account.
pub fn delete_profile_for_account(conn: &Connection, id: &str, owner_account: &str) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    let changed = transaction
        .execute(
            "DELETE FROM backup_profiles WHERE id = ?1 AND owner_account = ?2",
            params![id, owner_account],
        )
        .context("Failed to delete backup profile")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    // Also clean up related snapshots
    transaction
        .execute(
            "DELETE FROM file_snapshots WHERE profile_id = ?1",
            params![id],
        )
        .context("Failed to delete file snapshots for profile")?;
    transaction.commit()?;
    Ok(())
}

fn profile_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupProfile> {
    Ok(BackupProfile {
        id: row.get(0)?,
        owner_account: row.get(1)?,
        name: row.get(2)?,
        source_path: row.get(3)?,
        schedule: row.get(4)?,
        retention: row.get(5)?,
        folder: row.get(6)?,
        enabled: row.get::<_, i32>(7)? != 0,
        last_run: row.get(8)?,
        next_run: row.get(9)?,
        retry_count: row.get(10)?,
        retry_at: row.get(11)?,
        last_error: row.get(12)?,
        last_error_code: row.get(13)?,
        schedule_state: row.get(14)?,
        created_at: row.get(15)?,
    })
}

/// List all profiles for internal migrations. Runtime callers must use the
/// account-scoped variant below.
pub(crate) fn list_profiles(conn: &Connection) -> Result<Vec<BackupProfile>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, owner_account, name, source_path, schedule, retention, folder, enabled,
                    last_run, next_run, retry_count, retry_at, last_error, last_error_code,
                    schedule_state, created_at
             FROM backup_profiles
             ORDER BY created_at DESC",
        )
        .context("Failed to prepare profiles query")?;

    let rows = stmt
        .query_map([], profile_from_row)
        .context("Failed to execute profiles query")?;

    let mut profiles = Vec::new();
    for row in rows {
        profiles.push(row.context("Failed to read profile row")?);
    }
    Ok(profiles)
}

pub fn list_profiles_for_account(
    conn: &Connection,
    owner_account: &str,
) -> Result<Vec<BackupProfile>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, owner_account, name, source_path, schedule, retention, folder, enabled,
                    last_run, next_run, retry_count, retry_at, last_error, last_error_code,
                    schedule_state, created_at
             FROM backup_profiles
             WHERE owner_account = ?1
             ORDER BY created_at DESC",
        )
        .context("Failed to prepare account profiles query")?;
    let rows = stmt
        .query_map(params![owner_account], profile_from_row)
        .context("Failed to execute account profiles query")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to read account profile rows")
}

/// Get a profile for internal migrations and tests.
pub(crate) fn get_profile(conn: &Connection, id: &str) -> Result<BackupProfile> {
    conn.query_row(
        "SELECT id, owner_account, name, source_path, schedule, retention, folder, enabled,
                last_run, next_run, retry_count, retry_at, last_error, last_error_code,
                schedule_state, created_at
         FROM backup_profiles WHERE id = ?1",
        params![id],
        profile_from_row,
    )
    .with_context(|| format!("Profile not found: {}", id))
}

pub fn get_profile_for_account(
    conn: &Connection,
    id: &str,
    owner_account: &str,
) -> Result<BackupProfile> {
    conn.query_row(
        "SELECT id, owner_account, name, source_path, schedule, retention, folder, enabled,
                last_run, next_run, retry_count, retry_at, last_error, last_error_code,
                schedule_state, created_at
         FROM backup_profiles WHERE id = ?1 AND owner_account = ?2",
        params![id, owner_account],
        profile_from_row,
    )
    .map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => {
            anyhow!("Profile is not available for the signed-in account")
        }
        other => other.into(),
    })
}

pub fn count_unowned_profiles(conn: &Connection) -> Result<u64> {
    conn.query_row(
        "SELECT COUNT(*) FROM backup_profiles WHERE owner_account IS NULL",
        [],
        |row| row.get(0),
    )
    .context("Failed to count legacy profiles")
}

pub fn claim_unowned_profiles(conn: &Connection, owner_account: &str) -> Result<u64> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles SET owner_account = ?1 WHERE owner_account IS NULL",
            params![owner_account],
        )
        .context("Failed to claim legacy profiles")?;
    Ok(changed as u64)
}

/// Update just the last_run and next_run timestamps for a profile.
pub fn update_profile_run_times(
    conn: &Connection,
    id: &str,
    owner_account: &str,
    last_run: &str,
    next_run: Option<&str>,
) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles
            SET last_run = ?1, next_run = ?2, retry_count = 0, retry_at = NULL,
                last_error = NULL, last_error_code = NULL, schedule_state = 'scheduled'
          WHERE id = ?3 AND owner_account = ?4",
            params![last_run, next_run, id, owner_account],
        )
        .context("Failed to update profile run times")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    Ok(())
}

/// Persist the next bounded retry without advancing the regular schedule.
pub fn schedule_profile_retry(
    conn: &Connection,
    id: &str,
    owner_account: &str,
    retry_count: u32,
    retry_at: &str,
    error_code: &str,
    error_message: &str,
) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles
            SET retry_count = ?1, retry_at = ?2, last_error_code = ?3,
                last_error = ?4, schedule_state = 'retrying'
          WHERE id = ?5 AND owner_account = ?6",
            params![
                retry_count,
                retry_at,
                error_code,
                error_message,
                id,
                owner_account
            ],
        )
        .context("Failed to persist profile retry")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    Ok(())
}

/// Stop automatic retries for this occurrence and return to the next regular
/// schedule. The sanitized error code is retained for UI and telemetry.
pub fn mark_profile_needs_attention(
    conn: &Connection,
    id: &str,
    owner_account: &str,
    next_run: Option<&str>,
    retry_count: u32,
    error_code: &str,
    error_message: &str,
) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles
            SET next_run = ?1, retry_count = ?2, retry_at = NULL,
                last_error_code = ?3, last_error = ?4,
                schedule_state = 'needs_attention'
          WHERE id = ?5 AND owner_account = ?6",
            params![
                next_run,
                retry_count,
                error_code,
                error_message,
                id,
                owner_account
            ],
        )
        .context("Failed to mark profile as needing attention")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    Ok(())
}

/// A new regular occurrence starts a fresh retry budget while keeping the
/// previous error visible until the attempt succeeds or fails again.
pub fn begin_profile_attempt(conn: &Connection, id: &str, owner_account: &str) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE backup_profiles
            SET retry_count = 0, retry_at = NULL, schedule_state = 'scheduled'
          WHERE id = ?1 AND owner_account = ?2",
            params![id, owner_account],
        )
        .context("Failed to begin profile attempt")?;
    if changed != 1 {
        return Err(anyhow!(
            "Profile is not available for the signed-in account"
        ));
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// File snapshot functions
// ────────────────────────────────────────────────────────────────────

/// Get all file snapshots for a given profile.
pub fn get_file_snapshots(conn: &Connection, profile_id: &str) -> Result<Vec<FileSnapshot>> {
    let mut stmt = conn
        .prepare(
            "SELECT file_path, file_hash, file_size, modified_at, backup_id
             FROM file_snapshots
             WHERE profile_id = ?1",
        )
        .context("Failed to prepare snapshots query")?;

    let rows = stmt
        .query_map(params![profile_id], |row| {
            Ok(FileSnapshot {
                file_path: row.get(0)?,
                file_hash: row.get(1)?,
                file_size: row.get(2)?,
                modified_at: row.get(3)?,
                backup_id: row.get(4)?,
            })
        })
        .context("Failed to execute snapshots query")?;

    let mut snapshots = Vec::new();
    for row in rows {
        snapshots.push(row.context("Failed to read snapshot row")?);
    }
    Ok(snapshots)
}

/// Save file snapshots for a given profile, using UPSERT to replace existing entries.
pub fn save_file_snapshots(
    conn: &Connection,
    profile_id: &str,
    backup_id: &str,
    snapshots: &[FileSnapshot],
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("Failed to start transaction")?;

    for snap in snapshots {
        tx.execute(
            "INSERT INTO file_snapshots (profile_id, file_path, file_hash, file_size, modified_at, backup_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(profile_id, file_path) DO UPDATE SET
                file_hash = excluded.file_hash,
                file_size = excluded.file_size,
                modified_at = excluded.modified_at,
                backup_id = excluded.backup_id",
            params![
                profile_id,
                snap.file_path,
                snap.file_hash,
                snap.file_size,
                snap.modified_at,
                backup_id,
            ],
        )
        .context("Failed to upsert file snapshot")?;
    }

    tx.commit().context("Failed to commit snapshots")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> BackupProfile {
        BackupProfile {
            id: "profile-1".to_string(),
            owner_account: Some("owner@example.com".to_string()),
            name: "Test profile".to_string(),
            source_path: "C:\\test".to_string(),
            schedule: Some("daily".to_string()),
            retention: 7,
            folder: "/".to_string(),
            enabled: true,
            last_run: None,
            next_run: Some("2026-08-20T12:00:00Z".to_string()),
            retry_count: 0,
            retry_at: None,
            last_error: None,
            last_error_code: None,
            schedule_state: "scheduled".to_string(),
            created_at: "2026-08-19T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn profile_retry_state_survives_database_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        create_profile(&conn, &test_profile()).unwrap();

        schedule_profile_retry(
            &conn,
            "profile-1",
            "owner@example.com",
            1,
            "2026-08-19T12:05:00Z",
            "temporary_operation_failure",
            "local diagnostic",
        )
        .unwrap();
        let retrying = get_profile(&conn, "profile-1").unwrap();
        assert_eq!(retrying.schedule_state, "retrying");
        assert_eq!(retrying.retry_count, 1);
        assert_eq!(retrying.retry_at.as_deref(), Some("2026-08-19T12:05:00Z"));
        assert_eq!(retrying.last_error.as_deref(), Some("local diagnostic"));

        mark_profile_needs_attention(
            &conn,
            "profile-1",
            "owner@example.com",
            Some("2026-08-20T12:00:00Z"),
            3,
            "temporary_operation_failure",
            "local diagnostic",
        )
        .unwrap();
        let attention = get_profile(&conn, "profile-1").unwrap();
        assert_eq!(attention.schedule_state, "needs_attention");
        assert_eq!(attention.retry_count, 3);
        assert!(attention.retry_at.is_none());

        begin_profile_attempt(&conn, "profile-1", "owner@example.com").unwrap();
        let fresh_attempt = get_profile(&conn, "profile-1").unwrap();
        assert_eq!(fresh_attempt.schedule_state, "scheduled");
        assert_eq!(fresh_attempt.retry_count, 0);
        assert_eq!(
            fresh_attempt.last_error.as_deref(),
            Some("local diagnostic")
        );

        update_profile_run_times(
            &conn,
            "profile-1",
            "owner@example.com",
            "2026-08-20T12:00:01Z",
            Some("2026-08-21T12:00:00Z"),
        )
        .unwrap();
        let recovered = get_profile(&conn, "profile-1").unwrap();
        assert_eq!(recovered.schedule_state, "scheduled");
        assert!(recovered.last_error.is_none());
        assert!(recovered.last_error_code.is_none());
    }

    #[test]
    fn profiles_are_isolated_between_accounts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();

        let first = test_profile();
        let mut second = test_profile();
        second.id = "profile-2".to_string();
        second.owner_account = Some("other@example.com".to_string());
        second.name = "Other account profile".to_string();
        create_profile(&conn, &first).unwrap();
        create_profile(&conn, &second).unwrap();

        let first_account = list_profiles_for_account(&conn, "owner@example.com").unwrap();
        assert_eq!(first_account.len(), 1);
        assert_eq!(first_account[0].id, "profile-1");
        assert!(get_profile_for_account(&conn, "profile-2", "owner@example.com").is_err());
        assert!(delete_profile_for_account(&conn, "profile-2", "owner@example.com").is_err());
        assert!(update_profile_run_times(
            &conn,
            "profile-2",
            "owner@example.com",
            "2026-08-20T12:00:01Z",
            None,
        )
        .is_err());
        assert!(schedule_profile_retry(
            &conn,
            "profile-2",
            "owner@example.com",
            1,
            "2026-08-20T12:05:00Z",
            "temporary_operation_failure",
            "must not mutate",
        )
        .is_err());
        assert_eq!(
            get_profile_for_account(&conn, "profile-2", "other@example.com")
                .unwrap()
                .name,
            "Other account profile"
        );
    }

    #[test]
    fn legacy_profiles_are_paused_until_explicitly_claimed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let mut legacy = test_profile();
        legacy.owner_account = None;
        create_profile(&conn, &legacy).unwrap();

        assert_eq!(count_unowned_profiles(&conn).unwrap(), 1);
        assert!(list_profiles_for_account(&conn, "owner@example.com")
            .unwrap()
            .is_empty());
        assert!(get_profile_for_account(&conn, "profile-1", "owner@example.com").is_err());

        assert_eq!(
            claim_unowned_profiles(&conn, "owner@example.com").unwrap(),
            1
        );
        assert_eq!(count_unowned_profiles(&conn).unwrap(), 0);
        assert_eq!(
            list_profiles_for_account(&conn, "owner@example.com")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            claim_unowned_profiles(&conn, "other@example.com").unwrap(),
            0
        );
    }

    #[test]
    fn owner_migration_keeps_legacy_profiles_unclaimed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE backup_profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source_path TEXT NOT NULL,
                created_at TEXT NOT NULL
             );
             INSERT INTO backup_profiles (id, name, source_path, created_at)
             VALUES ('legacy', 'Legacy', 'C:\\test', '2026-08-19T12:00:00Z');",
        )
        .unwrap();

        migrate_profile_ownership(&conn).unwrap();
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner_account FROM backup_profiles WHERE id = 'legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(owner.is_none());
    }

    #[test]
    fn resilience_upgrade_normalizes_existing_disabled_profiles() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE backup_profiles (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source_path TEXT NOT NULL,
                schedule TEXT,
                retention INTEGER DEFAULT 0,
                folder TEXT NOT NULL DEFAULT '/',
                enabled INTEGER DEFAULT 1,
                last_run TEXT,
                next_run TEXT,
                created_at TEXT NOT NULL
             );
             INSERT INTO backup_profiles
                (id, name, source_path, schedule, enabled, next_run, created_at)
             VALUES
                ('disabled-profile', 'Disabled', 'C:\\test', 'daily', 0,
                 '2026-08-20T12:00:00Z', '2026-08-19T12:00:00Z');",
        )
        .unwrap();
        for migration in PROFILE_RESILIENCE_MIGRATIONS {
            conn.execute(migration, []).unwrap();
        }
        normalize_disabled_profile_state(&conn).unwrap();

        let state: (Option<String>, u32, Option<String>, Option<String>, Option<String>, String) = conn
            .query_row(
                "SELECT next_run, retry_count, retry_at, last_error, last_error_code, schedule_state
                 FROM backup_profiles WHERE id = 'disabled-profile'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();
        assert_eq!(state, (None, 0, None, None, None, "disabled".to_string()));
    }
}
