use crate::api::{
    OrganizationControlAcknowledgement, OrganizationControlCommand,
    OrganizationControlOrganization, ORGANIZATION_CONTROL_SCHEMA_VERSION,
};
use crate::backup_operations::AccountContext;
use crate::organization_enrollment::ManagedDeviceContext;
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tauri::Manager;

const CONTROL_POLL_LIMIT: u8 = 10;
pub(crate) const CONTROL_INITIAL_DELAY_SECONDS: u64 = 10;
pub(crate) const CONTROL_IDLE_INTERVAL_SECONDS: u64 = 30;
const CONTROL_MAX_BACKOFF_SECONDS: u64 = 300;
const MAX_PENDING_ACKS: usize = 10;
const MAX_PENDING_EVENTS: usize = 50;
const MAX_EVENT_BODY_BYTES: usize = 60 * 1024;

pub(crate) fn control_poll_delay_seconds(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return CONTROL_IDLE_INTERVAL_SECONDS;
    }
    CONTROL_IDLE_INTERVAL_SECONDS
        .saturating_mul(1_u64 << consecutive_failures.min(4))
        .min(CONTROL_MAX_BACKOFF_SECONDS)
}

const MANAGED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS managed_backup_policies (
    installation_id TEXT NOT NULL,
    assignment_id TEXT NOT NULL,
    policy_id TEXT NOT NULL,
    owner_account TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    organization_name TEXT NOT NULL,
    revision INTEGER NOT NULL,
    state_generation INTEGER NOT NULL,
    definition_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    source_path TEXT NOT NULL,
    schedule TEXT,
    retention INTEGER NOT NULL DEFAULT 0,
    desired_state TEXT NOT NULL,
    folder TEXT NOT NULL DEFAULT '/',
    last_run TEXT,
    next_run TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    retry_at TEXT,
    last_error_code TEXT,
    schedule_state TEXT NOT NULL DEFAULT 'scheduled',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (installation_id, assignment_id)
);

CREATE INDEX IF NOT EXISTS idx_managed_policies_owner
ON managed_backup_policies(owner_account, installation_id, desired_state);

CREATE TABLE IF NOT EXISTS managed_command_receipts (
    command_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    command_hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    ack_status TEXT NOT NULL,
    ack_result TEXT,
    retryable INTEGER,
    completed_at TEXT NOT NULL,
    ack_sent INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_managed_receipts_pending
ON managed_command_receipts(installation_id, ack_sent, completed_at);

CREATE TABLE IF NOT EXISTS managed_jobs (
    job_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    owner_account TEXT NOT NULL,
    command_id TEXT,
    kind TEXT NOT NULL,
    trigger_kind TEXT NOT NULL,
    assignment_id TEXT,
    policy_id TEXT,
    policy_revision INTEGER,
    policy_state_generation INTEGER,
    snapshot_id TEXT,
    destination_path TEXT,
    restore_staging_path TEXT,
    status TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    result_snapshot_id TEXT,
    error_code TEXT,
    UNIQUE(command_id)
);

CREATE INDEX IF NOT EXISTS idx_managed_jobs_dispatch
ON managed_jobs(installation_id, owner_account, status, created_at);

CREATE TABLE IF NOT EXISTS managed_control_events (
    event_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL,
    sent_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_managed_events_pending
ON managed_control_events(installation_id, sent_at, created_at);

CREATE TABLE IF NOT EXISTS managed_poll_state (
    installation_id TEXT PRIMARY KEY,
    poll_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS managed_disconnect_state (
    installation_id TEXT PRIMARY KEY,
    owner_account TEXT NOT NULL,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct ManagedBackupPolicy {
    pub assignment_id: String,
    pub policy_id: String,
    pub revision: i64,
    pub state_generation: i64,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub schedule: Option<String>,
    pub retention: i64,
    pub desired_state: String,
    pub folder: String,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    pub retry_count: u32,
    pub retry_at: Option<String>,
    pub last_error_code: Option<String>,
    pub schedule_state: String,
    pub organization_id: String,
    pub organization_name: String,
    pub managed: bool,
}

impl ManagedBackupPolicy {
    fn enabled(&self) -> bool {
        self.desired_state == "active"
    }

    fn profile_id(&self) -> &str {
        &self.assignment_id
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicySchedule {
    times: Vec<String>,
    interval_days: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyDefinition {
    schema_version: u32,
    kind: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    source_path: String,
    schedule: PolicySchedule,
    retention_snapshots: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicySyncPayload {
    schema_version: u32,
    operation: String,
    assignment_id: String,
    policy_id: String,
    revision: i64,
    state_generation: i64,
    desired_state: String,
    #[serde(default)]
    definition: Option<PolicyDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyActionPayload {
    schema_version: u32,
    assignment_id: String,
    policy_id: String,
    revision: i64,
    state_generation: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelBackupPayload {
    schema_version: u32,
    job_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreSnapshotPayload {
    schema_version: u32,
    snapshot_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteSnapshotPayload {
    schema_version: u32,
    snapshot_id: String,
}

#[derive(Debug, Clone)]
struct ManagedJob {
    job_id: String,
    installation_id: String,
    owner_account: String,
    command_id: Option<String>,
    kind: String,
    trigger_kind: String,
    assignment_id: Option<String>,
    policy_id: Option<String>,
    policy_revision: Option<i64>,
    policy_state_generation: Option<i64>,
    snapshot_id: Option<String>,
    destination_path: Option<String>,
    restore_staging_path: Option<String>,
    cancel_requested: bool,
}

#[derive(Debug)]
struct ActionOutcome {
    status: &'static str,
    result: Value,
    retryable: bool,
    cancel_job_id: Option<String>,
}

impl ActionOutcome {
    fn succeeded(result: Value) -> Self {
        Self {
            status: "succeeded",
            result,
            retryable: false,
            cancel_job_id: None,
        }
    }

    fn failed(code: &str, retryable: bool) -> Self {
        Self {
            status: "failed",
            result: json!({ "code": code }),
            retryable,
            cancel_job_id: None,
        }
    }
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|candidate| candidate == column))
}

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(MANAGED_SCHEMA)
        .context("Failed to initialize managed-device storage")?;
    if !table_has_column(conn, "managed_backup_policies", "state_generation")? {
        conn.execute(
            "ALTER TABLE managed_backup_policies
             ADD COLUMN state_generation INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    if !table_has_column(conn, "managed_jobs", "policy_state_generation")? {
        conn.execute(
            "ALTER TABLE managed_jobs ADD COLUMN policy_state_generation INTEGER",
            [],
        )?;
    }
    if !table_has_column(conn, "managed_jobs", "restore_staging_path")? {
        conn.execute(
            "ALTER TABLE managed_jobs ADD COLUMN restore_staging_path TEXT",
            [],
        )?;
    }
    Ok(())
}

fn validate_schema(version: u32) -> std::result::Result<(), ActionOutcome> {
    if version == ORGANIZATION_CONTROL_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ActionOutcome::failed("unsupported_schema_version", false))
    }
}

fn valid_state_generation(value: i64) -> bool {
    (1..=i32::MAX as i64).contains(&value)
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes[14].is_ascii_digit()
        && (b'1'..=b'8').contains(&bytes[14])
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && uuid::Uuid::parse_str(value).is_ok()
}

fn validate_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && value.len() <= 160
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '-')
        })
}

fn validate_snapshot_id(value: &str) -> bool {
    validate_identifier(value)
}

fn exact_snapshot<'a>(
    snapshots: &'a [crate::kopia::KopiaSnapshot],
    snapshot_id: &str,
) -> Option<&'a crate::kopia::KopiaSnapshot> {
    snapshots.iter().find(|snapshot| snapshot.id == snapshot_id)
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn contains_json_unsafe_text(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

fn contains_windows_path_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| character <= '\u{001f}' || character == '\u{007f}')
}

fn validate_windows_absolute_path(value: &str) -> bool {
    let value = value.trim();
    if utf16_len(value) < 3 || utf16_len(value) > 1024 || contains_windows_path_control(value) {
        return false;
    }
    // Keep this acceptance set byte-for-byte aligned with the Core API's
    // normalizedPath validator. Forward slashes and alternate device
    // namespaces are deliberately rejected instead of silently normalized.
    let segments: Vec<&str> = if value.len() >= 7
        && value.as_bytes().starts_with(b"\\\\?\\")
        && value.as_bytes().get(4).is_some_and(u8::is_ascii_alphabetic)
        && value.as_bytes().get(5) == Some(&b':')
        && value.as_bytes().get(6) == Some(&b'\\')
    {
        value[7..].split('\\').collect()
    } else if value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("\\\\?\\UNC\\"))
    {
        let segments: Vec<_> = value[8..].split('\\').collect();
        if segments.len() < 2 || segments[0].is_empty() || segments[1].is_empty() {
            return false;
        }
        segments
    } else if value.len() >= 3
        && value.as_bytes().get(0).is_some_and(u8::is_ascii_alphabetic)
        && value.as_bytes().get(1) == Some(&b':')
        && value.as_bytes().get(2) == Some(&b'\\')
    {
        value[3..].split('\\').collect()
    } else if value.starts_with("\\\\")
        && !value.starts_with("\\\\?\\")
        && !value.starts_with("\\\\.\\")
    {
        let segments: Vec<_> = value[2..].split('\\').collect();
        if segments.len() < 2 || segments[0].is_empty() || segments[1].is_empty() {
            return false;
        }
        segments
    } else {
        return false;
    };

    !segments[..segments.len().saturating_sub(1)]
        .iter()
        .any(|segment| segment.is_empty())
        && !segments.iter().any(|segment| {
            matches!(*segment, "." | "..")
                || segment
                    .chars()
                    .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        })
}

fn validate_windows_local_absolute_path(value: &str) -> bool {
    let local_drive = value.len() >= 3
        && value.as_bytes().get(0).is_some_and(u8::is_ascii_alphabetic)
        && value.as_bytes().get(1) == Some(&b':')
        && value.as_bytes().get(2) == Some(&b'\\');
    let extended_local_drive = value.len() >= 7
        && value.as_bytes().starts_with(b"\\\\?\\")
        && value.as_bytes().get(4).is_some_and(u8::is_ascii_alphabetic)
        && value.as_bytes().get(5) == Some(&b':')
        && value.as_bytes().get(6) == Some(&b'\\');
    (local_drive || extended_local_drive) && validate_windows_absolute_path(value)
}

fn validate_managed_source_path_syntax(value: &str) -> bool {
    validate_windows_local_absolute_path(value)
}

fn classified_drive_type_is_local(drive_type: u32) -> bool {
    // Win32 DRIVE_REMOVABLE and DRIVE_FIXED. Unknown/no-root, remote, CD-ROM,
    // and RAM-disk roots are outside the v1 managed-source/restore boundary.
    matches!(drive_type, 2 | 3)
}

#[cfg(target_os = "windows")]
fn restore_destination_drive_is_local(value: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let root_len = if value.starts_with("\\\\?\\") { 7 } else { 3 };
    let Some(root) = value.get(..root_len) else {
        return false;
    };
    let mut wide: Vec<u16> = std::ffi::OsStr::new(root).encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and remains alive for the call.
    let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
    classified_drive_type_is_local(drive_type)
}

#[cfg(not(target_os = "windows"))]
fn restore_destination_drive_is_local(_value: &str) -> bool {
    true
}

fn validate_restore_destination(value: &str) -> std::result::Result<PathBuf, ActionOutcome> {
    let value = value.trim();
    if !validate_windows_local_absolute_path(value) {
        return Err(ActionOutcome::failed("invalid_restore_destination", false));
    }
    let destination = PathBuf::from(value);
    if destination.exists() {
        return Err(ActionOutcome::failed(
            "restore_destination_already_exists",
            false,
        ));
    }
    let Some(parent) = destination.parent() else {
        return Err(ActionOutcome::failed("invalid_restore_destination", false));
    };
    if !parent.is_dir() {
        return Err(ActionOutcome::failed(
            "restore_destination_parent_missing",
            false,
        ));
    }
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|_| ActionOutcome::failed("restore_destination_parent_unavailable", false))?;
    let canonical_parent_value = canonical_parent.to_string_lossy();
    if !validate_windows_local_absolute_path(&canonical_parent_value)
        || !restore_destination_drive_is_local(&canonical_parent_value)
    {
        return Err(ActionOutcome::failed(
            "restore_destination_remote_drive",
            false,
        ));
    }
    let Some(destination_name) = destination.file_name() else {
        return Err(ActionOutcome::failed("invalid_restore_destination", false));
    };
    let canonical_destination = canonical_parent.join(destination_name);
    if canonical_destination.exists() {
        return Err(ActionOutcome::failed(
            "restore_destination_already_exists",
            false,
        ));
    }
    Ok(canonical_destination)
}

fn normalized_schedule(schedule: &PolicySchedule) -> std::result::Result<String, ActionOutcome> {
    if schedule.interval_days == 0 || schedule.interval_days > 365 {
        return Err(ActionOutcome::failed("invalid_policy_schedule", false));
    }
    if schedule.times.is_empty() || schedule.times.len() > 24 {
        return Err(ActionOutcome::failed("invalid_policy_schedule", false));
    }
    let mut times = schedule.times.clone();
    times.sort();
    times.dedup();
    if times.len() != schedule.times.len()
        || times.iter().any(|time| {
            let bytes = time.as_bytes();
            bytes.len() != 5
                || bytes[2] != b':'
                || !bytes[..2].iter().all(u8::is_ascii_digit)
                || !bytes[3..].iter().all(u8::is_ascii_digit)
                || time[..2].parse::<u8>().map_or(true, |hour| hour > 23)
                || time[3..].parse::<u8>().map_or(true, |minute| minute > 59)
        })
    {
        return Err(ActionOutcome::failed("invalid_policy_schedule", false));
    }
    serde_json::to_string(&json!({
        "times": times,
        "intervalDays": schedule.interval_days,
    }))
    .map_err(|_| ActionOutcome::failed("invalid_policy_schedule", false))
}

fn validate_definition(
    definition: &PolicyDefinition,
) -> std::result::Result<(String, Option<String>, String, String, i64), ActionOutcome> {
    validate_schema(definition.schema_version)?;
    let name = definition.name.trim();
    if definition.kind != "folder"
        || utf16_len(name) == 0
        || utf16_len(name) > 80
        || contains_json_unsafe_text(name)
    {
        return Err(ActionOutcome::failed("invalid_policy_definition", false));
    }
    let description = definition
        .description
        .as_deref()
        .map(str::trim)
        .map(str::to_string);
    if description
        .as_ref()
        .is_some_and(|value| utf16_len(value) > 500 || contains_json_unsafe_text(value))
        || !validate_managed_source_path_syntax(&definition.source_path)
        || !(0..=365).contains(&definition.retention_snapshots)
    {
        return Err(ActionOutcome::failed("invalid_policy_definition", false));
    }
    let schedule = normalized_schedule(&definition.schedule)?;
    Ok((
        name.to_string(),
        description,
        definition.source_path.trim().to_string(),
        schedule,
        definition.retention_snapshots,
    ))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut normalized = serde_json::Map::new();
            for (key, value) in entries {
                normalized.insert(key.clone(), canonicalize(value));
            }
            Value::Object(normalized)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn command_hash(command: &OrganizationControlCommand) -> Result<String> {
    let serialized = serde_json::to_vec(&json!({
        "kind": command.kind,
        "payload": canonicalize(&command.payload),
    }))?;
    Ok(hex::encode(Sha256::digest(serialized)))
}

fn policy_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManagedBackupPolicy> {
    Ok(ManagedBackupPolicy {
        assignment_id: row.get(0)?,
        policy_id: row.get(1)?,
        revision: row.get::<_, i64>(2)?,
        state_generation: row.get::<_, i64>(3)?,
        name: row.get(4)?,
        description: row.get(5)?,
        source_path: row.get(6)?,
        schedule: row.get(7)?,
        retention: row.get(8)?,
        desired_state: row.get(9)?,
        folder: row.get(10)?,
        last_run: row.get(11)?,
        next_run: row.get(12)?,
        retry_count: row.get(13)?,
        retry_at: row.get(14)?,
        last_error_code: row.get(15)?,
        schedule_state: row.get(16)?,
        organization_id: row.get(17)?,
        organization_name: row.get(18)?,
        managed: true,
    })
}

fn list_policies(
    conn: &Connection,
    installation_id: &str,
    owner_account: &str,
) -> Result<Vec<ManagedBackupPolicy>> {
    let mut statement = conn.prepare(
        "SELECT assignment_id, policy_id, revision, state_generation, name, description, source_path,
                schedule, retention, desired_state, folder, last_run, next_run,
                retry_count, retry_at, last_error_code, schedule_state,
                organization_id, organization_name
           FROM managed_backup_policies
          WHERE installation_id = ?1 AND owner_account = ?2 AND desired_state <> 'removed'
          ORDER BY name COLLATE NOCASE, assignment_id",
    )?;
    let policies = statement
        .query_map(params![installation_id, owner_account], policy_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to read managed backup policies")?;
    Ok(policies)
}

fn load_policy(
    conn: &Connection,
    installation_id: &str,
    owner_account: &str,
    assignment_id: &str,
) -> Result<Option<ManagedBackupPolicy>> {
    conn.query_row(
        "SELECT assignment_id, policy_id, revision, state_generation, name, description, source_path,
                schedule, retention, desired_state, folder, last_run, next_run,
                retry_count, retry_at, last_error_code, schedule_state,
                organization_id, organization_name
           FROM managed_backup_policies
          WHERE installation_id = ?1 AND owner_account = ?2 AND assignment_id = ?3",
        params![installation_id, owner_account, assignment_id],
        policy_from_row,
    )
    .optional()
    .context("Failed to load managed backup policy")
}

#[tauri::command]
pub async fn cmd_list_managed_profiles(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<ManagedBackupPolicy>, String> {
    let Some((installation_id, owner_account)) =
        crate::organization_enrollment::managed_device_binding(state.inner())
            .map_err(|error| error.to_string())?
    else {
        return Ok(Vec::new());
    };
    let guard = state
        .0
        .lock()
        .map_err(|error| format!("Lock error: {error}"))?;
    list_policies(&guard.db, &installation_id, &owner_account).map_err(|error| error.to_string())
}

#[derive(Debug, Serialize)]
pub struct LocalManagedRestore {
    job_id: String,
    destination_path: String,
    status: String,
    created_at: String,
}

// This projection is intentionally local-only. A restore command, receipt, or
// event never accepts or sends a destination chosen by the administrator.
#[tauri::command]
pub async fn cmd_list_local_managed_restores(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<LocalManagedRestore>, String> {
    let guard = state
        .0
        .lock()
        .map_err(|error| format!("Lock error: {error}"))?;
    let owner_account = guard
        .account_scope()
        .ok_or_else(|| "Sign in to view managed restores".to_string())?;
    let mut statement = guard
        .db
        .prepare(
            "SELECT job_id, destination_path, status, created_at FROM managed_jobs
          WHERE owner_account = ?1 AND kind = 'restore' AND destination_path IS NOT NULL
          ORDER BY created_at DESC, job_id DESC LIMIT 20",
        )
        .map_err(|error| error.to_string())?;
    let restores = statement
        .query_map(params![owner_account], |row| {
            Ok(LocalManagedRestore {
                job_id: row.get(0)?,
                destination_path: row.get(1)?,
                status: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    Ok(restores)
}

fn stored_receipt(
    tx: &Transaction<'_>,
    command: &OrganizationControlCommand,
    hash: &str,
) -> Result<Option<OrganizationControlAcknowledgement>> {
    let existing: Option<(String, String, Option<String>, Option<i64>, String, String)> = tx
        .query_row(
            "SELECT command_hash, ack_status, ack_result, retryable, completed_at, lease_id
               FROM managed_command_receipts WHERE command_id = ?1",
            params![command.id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_hash, status, result, retryable, completed_at, stored_lease_id)) = existing
    else {
        return Ok(None);
    };
    if stored_hash != hash {
        return Ok(Some(OrganizationControlAcknowledgement {
            command_id: command.id.clone(),
            lease_id: command.lease_id.clone(),
            status: "failed".to_string(),
            completed_at: Utc::now().to_rfc3339(),
            result: Some(json!({ "code": "command_replay_conflict" })),
            retryable: Some(false),
        }));
    }
    if status == "failed" && retryable == Some(1) && stored_lease_id != command.lease_id {
        tx.execute(
            "DELETE FROM managed_command_receipts WHERE command_id = ?1",
            params![command.id],
        )?;
        return Ok(None);
    }
    tx.execute(
        "UPDATE managed_command_receipts SET lease_id = ?1, ack_sent = 0 WHERE command_id = ?2",
        params![command.lease_id, command.id],
    )?;
    Ok(Some(OrganizationControlAcknowledgement {
        command_id: command.id.clone(),
        lease_id: command.lease_id.clone(),
        status,
        completed_at,
        result: result
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("Stored managed command result is invalid")?,
        retryable: retryable.map(|value| value != 0),
    }))
}

fn definition_hash(definition: &PolicyDefinition) -> Result<String> {
    let value = serde_json::to_value(definition)?;
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(
        &canonicalize(&value),
    )?)))
}

fn apply_policy_sync(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    organization: &OrganizationControlOrganization,
    payload: &Value,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: PolicySyncPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_policy_sync", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_identifier(&payload.assignment_id)
        || !validate_identifier(&payload.policy_id)
        || payload.revision <= 0
        || payload.revision > i32::MAX as i64
        || !valid_state_generation(payload.state_generation)
        || !matches!(
            payload.desired_state.as_str(),
            "active" | "paused" | "removed"
        )
    {
        return Err(ActionOutcome::failed("invalid_policy_sync", false));
    }

    let existing: Option<(String, i64, i64, String, String)> = tx
        .query_row(
            "SELECT policy_id, revision, state_generation, definition_hash, desired_state
               FROM managed_backup_policies
              WHERE installation_id = ?1 AND assignment_id = ?2",
            params![context.installation_id, payload.assignment_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    if existing
        .as_ref()
        .is_some_and(|(_, _, generation, _, _)| *generation > payload.state_generation)
    {
        return Err(ActionOutcome::failed(
            "policy_state_generation_conflict",
            false,
        ));
    }
    if existing
        .as_ref()
        .is_some_and(|(_, revision, _, _, _)| *revision > payload.revision)
    {
        return Err(ActionOutcome::failed("policy_revision_conflict", false));
    }
    if existing
        .as_ref()
        .is_some_and(|(policy_id, _, _, _, _)| policy_id != &payload.policy_id)
    {
        return Err(ActionOutcome::failed("policy_assignment_conflict", false));
    }

    let now = Utc::now().to_rfc3339();
    match payload.operation.as_str() {
        "remove" => {
            if payload.desired_state != "removed" || payload.definition.is_some() {
                return Err(ActionOutcome::failed("invalid_policy_sync", false));
            }
            let tombstone_hash = hex::encode(Sha256::digest(format!(
                "removed:{}:{}:{}",
                payload.assignment_id, payload.policy_id, payload.revision
            )));
            if let Some((_, revision, generation, previous_hash, previous_state)) =
                existing.as_ref()
            {
                if *generation == payload.state_generation {
                    if *revision == payload.revision
                        && previous_hash == &tombstone_hash
                        && previous_state == "removed"
                    {
                        return Ok(ActionOutcome::succeeded(json!({
                            "appliedRevision": payload.revision,
                            "appliedStateGeneration": payload.state_generation,
                        })));
                    }
                    return Err(ActionOutcome::failed(
                        "policy_state_generation_conflict",
                        false,
                    ));
                }
            }
            tx.execute(
                "INSERT INTO managed_backup_policies
                 (installation_id, assignment_id, policy_id, owner_account,
                  organization_id, organization_name, revision, state_generation, definition_hash,
                  name, description, source_path, schedule, retention, desired_state,
                  folder, last_run, next_run, retry_count, retry_at, last_error_code,
                  schedule_state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '', NULL, '', NULL, 0,
                         'removed', '/', NULL, NULL, 0, NULL, NULL, 'removed', ?10, ?10)
                 ON CONFLICT(installation_id, assignment_id) DO UPDATE SET
                  revision = excluded.revision, state_generation = excluded.state_generation,
                  definition_hash = excluded.definition_hash,
                  organization_id = excluded.organization_id,
                  organization_name = excluded.organization_name, name = '', description = NULL,
                  source_path = '', schedule = NULL, retention = 0,
                  desired_state = 'removed', next_run = NULL, retry_count = 0,
                  retry_at = NULL, last_error_code = NULL, schedule_state = 'removed',
                  updated_at = excluded.updated_at",
                params![
                    context.installation_id,
                    payload.assignment_id,
                    payload.policy_id,
                    context.account.account_scope,
                    organization.id,
                    organization.name,
                    payload.revision,
                    payload.state_generation,
                    tombstone_hash,
                    now,
                ],
            )
            .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
            tx.execute(
                "UPDATE managed_jobs SET cancel_requested = 1
                  WHERE installation_id = ?1 AND assignment_id = ?2
                    AND status IN ('queued','running')",
                params![context.installation_id, payload.assignment_id],
            )
            .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
            Ok(ActionOutcome::succeeded(json!({
                "appliedRevision": payload.revision,
                "appliedStateGeneration": payload.state_generation,
            })))
        }
        "upsert" => {
            if payload.desired_state == "removed" {
                return Err(ActionOutcome::failed("invalid_policy_sync", false));
            }
            let definition = payload
                .definition
                .as_ref()
                .ok_or_else(|| ActionOutcome::failed("invalid_policy_definition", false))?;
            let (name, description, source_path, schedule, retention) =
                validate_definition(definition)?;
            let definition_hash = definition_hash(definition)
                .map_err(|_| ActionOutcome::failed("invalid_policy_definition", false))?;
            if let Some((_, revision, generation, previous_hash, previous_state)) =
                existing.as_ref()
            {
                if *generation == payload.state_generation {
                    if *revision == payload.revision
                        && previous_hash == &definition_hash
                        && previous_state == &payload.desired_state
                    {
                        return Ok(ActionOutcome::succeeded(json!({
                            "appliedRevision": payload.revision,
                            "appliedStateGeneration": payload.state_generation,
                        })));
                    }
                    return Err(ActionOutcome::failed(
                        "policy_state_generation_conflict",
                        false,
                    ));
                }
            }
            let active = payload.desired_state == "active";
            let next_run = active
                .then(|| crate::profiles::compute_next_run(Some(&schedule)))
                .flatten();
            let schedule_state = if active { "scheduled" } else { "paused" };
            tx.execute(
                "INSERT INTO managed_backup_policies
                 (installation_id, assignment_id, policy_id, owner_account,
                  organization_id, organization_name, revision, state_generation, definition_hash,
                  name, description, source_path, schedule, retention, desired_state,
                  folder, last_run, next_run, retry_count, retry_at, last_error_code,
                  schedule_state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, '/', NULL, ?16, 0, NULL, NULL, ?17, ?18, ?18)
                 ON CONFLICT(installation_id, assignment_id) DO UPDATE SET
                  organization_id = excluded.organization_id,
                  organization_name = excluded.organization_name,
                  revision = excluded.revision, state_generation = excluded.state_generation,
                  definition_hash = excluded.definition_hash,
                  name = excluded.name, description = excluded.description,
                  source_path = excluded.source_path, schedule = excluded.schedule,
                  retention = excluded.retention, desired_state = excluded.desired_state,
                  next_run = excluded.next_run, retry_count = 0, retry_at = NULL,
                  last_error_code = NULL, schedule_state = excluded.schedule_state,
                  updated_at = excluded.updated_at",
                params![
                    context.installation_id,
                    payload.assignment_id,
                    payload.policy_id,
                    context.account.account_scope,
                    organization.id,
                    organization.name,
                    payload.revision,
                    payload.state_generation,
                    definition_hash,
                    name,
                    description,
                    source_path,
                    schedule,
                    retention,
                    payload.desired_state,
                    next_run,
                    schedule_state,
                    now,
                ],
            )
            .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
            tx.execute(
                "UPDATE managed_jobs SET cancel_requested = 1
                  WHERE installation_id = ?1 AND assignment_id = ?2
                    AND status IN ('queued','running')
                    AND (policy_revision <> ?3 OR policy_state_generation IS NULL
                         OR policy_state_generation <> ?4
                         OR (?5 = 'paused' AND status = 'queued'))",
                params![
                    context.installation_id,
                    payload.assignment_id,
                    payload.revision,
                    payload.state_generation,
                    payload.desired_state,
                ],
            )
            .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
            Ok(ActionOutcome::succeeded(json!({
                "appliedRevision": payload.revision,
                "appliedStateGeneration": payload.state_generation,
            })))
        }
        _ => Err(ActionOutcome::failed("invalid_policy_sync", false)),
    }
}

fn update_policy_state(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    payload: &Value,
    desired_state: &'static str,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: PolicyActionPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_policy_action", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_identifier(&payload.assignment_id)
        || !validate_identifier(&payload.policy_id)
        || payload.revision <= 0
        || payload.revision > i32::MAX as i64
        || !valid_state_generation(payload.state_generation)
    {
        return Err(ActionOutcome::failed("invalid_policy_action", false));
    }
    let current: Option<(String, i64, i64, String, Option<String>)> = tx
        .query_row(
            "SELECT policy_id, revision, state_generation, desired_state, schedule
               FROM managed_backup_policies
              WHERE installation_id = ?1 AND owner_account = ?2 AND assignment_id = ?3",
            params![
                context.installation_id,
                context.account.account_scope,
                payload.assignment_id
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    let Some((policy_id, revision, state_generation, current_state, schedule)) = current else {
        return Err(ActionOutcome::failed("managed_policy_not_found", false));
    };
    if policy_id != payload.policy_id || revision != payload.revision {
        return Err(ActionOutcome::failed("policy_revision_conflict", false));
    }
    if payload.state_generation < state_generation {
        return Err(ActionOutcome::failed(
            "policy_state_generation_conflict",
            false,
        ));
    }
    if current_state == "removed" {
        return Err(ActionOutcome::failed("managed_policy_removed", false));
    }
    if payload.state_generation == state_generation {
        if current_state == desired_state {
            return Ok(ActionOutcome::succeeded(json!({
                "appliedRevision": payload.revision,
                "appliedStateGeneration": payload.state_generation,
            })));
        }
        return Err(ActionOutcome::failed(
            "policy_state_generation_conflict",
            false,
        ));
    }
    let next_run = if desired_state == "active" {
        schedule
            .as_deref()
            .and_then(|value| crate::profiles::compute_next_run(Some(value)))
    } else {
        None
    };
    tx.execute(
        "UPDATE managed_backup_policies
            SET desired_state = ?1, state_generation = ?2, next_run = ?3,
                retry_count = 0, retry_at = NULL, last_error_code = NULL,
                schedule_state = ?4, updated_at = ?5
          WHERE installation_id = ?6 AND owner_account = ?7 AND assignment_id = ?8",
        params![
            desired_state,
            payload.state_generation,
            next_run,
            if desired_state == "active" {
                "scheduled"
            } else {
                "paused"
            },
            Utc::now().to_rfc3339(),
            context.installation_id,
            context.account.account_scope,
            payload.assignment_id,
        ],
    )
    .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    tx.execute(
        "UPDATE managed_jobs SET cancel_requested = 1
          WHERE installation_id = ?1 AND assignment_id = ?2
            AND status IN ('queued','running')
            AND (policy_state_generation IS NULL OR policy_state_generation <> ?3)",
        params![
            context.installation_id,
            payload.assignment_id,
            payload.state_generation,
        ],
    )
    .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    Ok(ActionOutcome::succeeded(json!({
        "appliedRevision": payload.revision,
        "appliedStateGeneration": payload.state_generation,
    })))
}

fn insert_job(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    command_id: Option<&str>,
    kind: &str,
    trigger_kind: &str,
    assignment_id: Option<&str>,
    policy_id: Option<&str>,
    policy_revision: Option<i64>,
    policy_state_generation: Option<i64>,
    snapshot_id: Option<&str>,
    destination_path: Option<&str>,
) -> std::result::Result<String, ActionOutcome> {
    let job_id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO managed_jobs
         (job_id, installation_id, owner_account, command_id, kind, trigger_kind,
          assignment_id, policy_id, policy_revision, policy_state_generation,
          snapshot_id, destination_path,
          status, cancel_requested, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued', 0, ?13)",
        params![
            job_id,
            context.installation_id,
            context.account.account_scope,
            command_id,
            kind,
            trigger_kind,
            assignment_id,
            policy_id,
            policy_revision,
            policy_state_generation,
            snapshot_id,
            destination_path,
            Utc::now().to_rfc3339(),
        ],
    )
    .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    queue_initial_job_event(tx, &job_id)
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    Ok(job_id)
}

fn queue_run_backup(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    command_id: &str,
    payload: &Value,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: PolicyActionPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_run_backup", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_identifier(&payload.assignment_id)
        || !validate_identifier(&payload.policy_id)
        || payload.revision <= 0
        || payload.revision > i32::MAX as i64
        || !valid_state_generation(payload.state_generation)
    {
        return Err(ActionOutcome::failed("invalid_run_backup", false));
    }
    let current: Option<(String, i64, i64, String)> = tx
        .query_row(
            "SELECT policy_id, revision, state_generation, desired_state
               FROM managed_backup_policies
              WHERE installation_id = ?1 AND owner_account = ?2 AND assignment_id = ?3",
            params![
                context.installation_id,
                context.account.account_scope,
                payload.assignment_id
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    let Some((policy_id, revision, state_generation, desired_state)) = current else {
        return Err(ActionOutcome::failed("managed_policy_not_found", false));
    };
    if policy_id != payload.policy_id || revision != payload.revision {
        return Err(ActionOutcome::failed("policy_revision_conflict", false));
    }
    if state_generation != payload.state_generation {
        return Err(ActionOutcome::failed(
            "policy_state_generation_conflict",
            false,
        ));
    }
    if desired_state != "active" {
        return Err(ActionOutcome::failed("managed_policy_not_active", false));
    }
    let job_id = insert_job(
        tx,
        context,
        Some(command_id),
        "backup",
        "managed_manual",
        Some(&payload.assignment_id),
        Some(&payload.policy_id),
        Some(payload.revision),
        Some(payload.state_generation),
        None,
        None,
    )?;
    Ok(ActionOutcome::succeeded(json!({
        "jobId": job_id,
        "appliedRevision": payload.revision,
        "appliedStateGeneration": payload.state_generation,
    })))
}

fn queue_restore(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    command_id: &str,
    payload: &Value,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: RestoreSnapshotPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_restore_snapshot", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_snapshot_id(&payload.snapshot_id) {
        return Err(ActionOutcome::failed("invalid_restore_snapshot", false));
    }
    let destination = managed_restore_destination_candidate();
    let job_id = insert_job(
        tx,
        context,
        Some(command_id),
        "restore",
        "managed_restore",
        None,
        None,
        None,
        None,
        Some(&payload.snapshot_id),
        Some(&destination.to_string_lossy()),
    )?;
    Ok(ActionOutcome::succeeded(json!({ "jobId": job_id })))
}

fn queue_delete_snapshot(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    command_id: &str,
    payload: &Value,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: DeleteSnapshotPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_delete_snapshot", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_snapshot_id(&payload.snapshot_id) {
        return Err(ActionOutcome::failed("invalid_delete_snapshot", false));
    }
    let job_id = insert_job(
        tx,
        context,
        Some(command_id),
        "delete_snapshot",
        "managed_delete",
        None,
        None,
        None,
        None,
        Some(&payload.snapshot_id),
        None,
    )?;
    Ok(ActionOutcome::succeeded(json!({ "jobId": job_id })))
}

fn request_cancel(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    payload: &Value,
) -> std::result::Result<ActionOutcome, ActionOutcome> {
    let payload: CancelBackupPayload = serde_json::from_value(payload.clone())
        .map_err(|_| ActionOutcome::failed("invalid_cancel_backup", false))?;
    validate_schema(payload.schema_version)?;
    if !validate_identifier(&payload.job_id) {
        return Err(ActionOutcome::failed("invalid_cancel_backup", false));
    }
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM managed_jobs
              WHERE job_id = ?1 AND installation_id = ?2 AND kind = 'backup'",
            params![payload.job_id, context.installation_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    let Some(status) = status else {
        return Err(ActionOutcome::failed("managed_job_not_found", false));
    };
    let already_terminal = matches!(
        status.as_str(),
        "succeeded" | "failed" | "cancelled" | "interrupted"
    );
    if !already_terminal {
        tx.execute(
            "UPDATE managed_jobs SET cancel_requested = 1 WHERE job_id = ?1",
            params![payload.job_id],
        )
        .map_err(|_| ActionOutcome::failed("local_storage_unavailable", true))?;
    }
    let mut outcome = ActionOutcome::succeeded(json!({ "jobId": payload.job_id }));
    if !already_terminal {
        outcome.cancel_job_id = Some(payload.job_id);
    }
    Ok(outcome)
}

fn execute_action(
    tx: &Transaction<'_>,
    context: &ManagedDeviceContext,
    organization: &OrganizationControlOrganization,
    command: &OrganizationControlCommand,
) -> ActionOutcome {
    let result = match command.kind.as_str() {
        "policy_sync" => apply_policy_sync(tx, context, organization, &command.payload),
        "run_backup" => queue_run_backup(tx, context, &command.id, &command.payload),
        "cancel_backup" => request_cancel(tx, context, &command.payload),
        "restore_snapshot" => queue_restore(tx, context, &command.id, &command.payload),
        "delete_snapshot" => queue_delete_snapshot(tx, context, &command.id, &command.payload),
        "pause" => update_policy_state(tx, context, &command.payload, "paused"),
        "resume" => update_policy_state(tx, context, &command.payload, "active"),
        _ => Err(ActionOutcome::failed("unsupported_command_kind", false)),
    };
    result.unwrap_or_else(|failure| failure)
}

fn valid_command_envelope(
    command: &OrganizationControlCommand,
    expected_lease_id: Option<&str>,
) -> bool {
    let created_at = DateTime::parse_from_rfc3339(&command.created_at);
    let expires_at = DateTime::parse_from_rfc3339(&command.expires_at);
    validate_identifier(&command.id)
        && valid_uuid(&command.lease_id)
        && expected_lease_id.map_or(true, |expected| command.lease_id == expected)
        && command.attempt > 0
        && command.max_attempts > 0
        && command.max_attempts <= 10
        && command.attempt <= command.max_attempts
        && created_at
            .as_ref()
            .ok()
            .zip(expires_at.as_ref().ok())
            .is_some_and(|(created, expires)| created <= expires)
}

fn process_command(
    conn: &Connection,
    context: &ManagedDeviceContext,
    organization: &OrganizationControlOrganization,
    command: &OrganizationControlCommand,
) -> Result<(OrganizationControlAcknowledgement, Option<String>)> {
    if !valid_command_envelope(command, None) {
        return Err(anyhow!("Invalid organization control command envelope"));
    }
    let hash = command_hash(command)?;
    let tx = conn.unchecked_transaction()?;
    if let Some(acknowledgement) = stored_receipt(&tx, command, &hash)? {
        tx.commit()?;
        return Ok((acknowledgement, None));
    }
    let expired = DateTime::parse_from_rfc3339(&command.expires_at)
        .map(|value| value.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(true);
    let outcome = if expired {
        ActionOutcome::failed("command_expired", false)
    } else {
        execute_action(&tx, context, organization, command)
    };
    let completed_at = Utc::now().to_rfc3339();
    let result_json = serde_json::to_string(&outcome.result)?;
    tx.execute(
        "INSERT INTO managed_command_receipts
         (command_id, installation_id, command_hash, kind, lease_id, ack_status,
          ack_result, retryable, completed_at, ack_sent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
        params![
            command.id,
            context.installation_id,
            hash,
            command.kind,
            command.lease_id,
            outcome.status,
            result_json,
            (outcome.status == "failed").then_some(outcome.retryable as i32),
            completed_at,
        ],
    )?;
    tx.commit()?;
    Ok((
        OrganizationControlAcknowledgement {
            command_id: command.id.clone(),
            lease_id: command.lease_id.clone(),
            status: outcome.status.to_string(),
            completed_at,
            result: Some(outcome.result),
            retryable: (outcome.status == "failed").then_some(outcome.retryable),
        },
        outcome.cancel_job_id,
    ))
}

fn job_kind_for_event(kind: &str) -> Option<&'static str> {
    match kind {
        "backup" => Some("backup"),
        "restore" => Some("restore"),
        "delete_snapshot" => Some("snapshot_delete"),
        _ => None,
    }
}

fn queue_job_event_tx(
    tx: &Transaction<'_>,
    job: &ManagedJob,
    event_type: &str,
    snapshot_id: Option<&str>,
    error_code: Option<&str>,
) -> Result<()> {
    let job_kind =
        job_kind_for_event(&job.kind).ok_or_else(|| anyhow!("Unsupported managed job kind"))?;
    let mut object = serde_json::Map::new();
    object.insert("eventId".into(), json!(uuid::Uuid::new_v4().to_string()));
    object.insert("type".into(), json!(event_type));
    object.insert("occurredAt".into(), json!(Utc::now().to_rfc3339()));
    object.insert("jobId".into(), json!(job.job_id));
    object.insert("jobKind".into(), json!(job_kind));
    if let Some(command_id) = job.command_id.as_deref() {
        object.insert("commandId".into(), json!(command_id));
    }
    if let Some(assignment_id) = job.assignment_id.as_deref() {
        object.insert("assignmentId".into(), json!(assignment_id));
    }
    if let Some(policy_id) = job.policy_id.as_deref() {
        object.insert("policyId".into(), json!(policy_id));
    }
    if let Some(revision) = job.policy_revision {
        object.insert("policyRevision".into(), json!(revision));
    }
    if let Some(snapshot_id) = snapshot_id {
        object.insert("snapshotId".into(), json!(snapshot_id));
    }
    if let Some(error_code) = error_code {
        object.insert("errorCode".into(), json!(error_code));
    }
    queue_event_value_tx(tx, &job.installation_id, Value::Object(object))
}

fn queue_snapshot_event_tx(
    tx: &Transaction<'_>,
    job: &ManagedJob,
    event_type: &str,
    snapshot_id: &str,
    snapshot: Option<&crate::kopia::KopiaSnapshot>,
    deletion_reason: Option<&str>,
) -> Result<()> {
    let job_kind = job_kind_for_event(&job.kind)
        .ok_or_else(|| anyhow!("Unsupported managed snapshot job kind"))?;
    let mut object = serde_json::Map::new();
    object.insert("eventId".into(), json!(uuid::Uuid::new_v4().to_string()));
    object.insert("type".into(), json!(event_type));
    let occurred_at = Utc::now().to_rfc3339();
    object.insert("occurredAt".into(), json!(occurred_at));
    object.insert("jobId".into(), json!(job.job_id));
    object.insert("jobKind".into(), json!(job_kind));
    object.insert("snapshotId".into(), json!(snapshot_id));
    if event_type == "snapshot_available" {
        if deletion_reason.is_some() || job_kind != "backup" {
            return Err(anyhow!("Invalid managed snapshot-available event"));
        }
        object.insert(
            "snapshotAt".into(),
            json!(snapshot
                .map(|value| value.start_time.as_str())
                .filter(|value| !value.is_empty())
                .unwrap_or(&occurred_at)),
        );
        if let Some(snapshot) = snapshot {
            object.insert("logicalBytes".into(), json!(snapshot.size));
            object.insert("fileCount".into(), json!(snapshot.file_count));
        }
        if let Some(command_id) = job.command_id.as_deref() {
            object.insert("commandId".into(), json!(command_id));
        }
    } else if event_type == "snapshot_deleted" {
        let deletion_reason = deletion_reason
            .filter(|reason| matches!(*reason, "administrator" | "retention"))
            .ok_or_else(|| anyhow!("Managed snapshot deletion reason is invalid"))?;
        object.insert("deletionReason".into(), json!(deletion_reason));
        if deletion_reason == "administrator" {
            if job_kind != "snapshot_delete" {
                return Err(anyhow!("Administrator deletion requires a deletion job"));
            }
            let command_id = job
                .command_id
                .as_deref()
                .ok_or_else(|| anyhow!("Administrator deletion requires a command"))?;
            object.insert("commandId".into(), json!(command_id));
        } else {
            // Retention may run during either scheduled or administrator-started
            // backups, but its deletion event is policy-scoped rather than
            // command-scoped and therefore deliberately omits commandId.
            if job_kind != "backup"
                || job.assignment_id.is_none()
                || job.policy_id.is_none()
                || job.policy_revision.is_none()
            {
                return Err(anyhow!("Retention deletion requires a backup job"));
            }
        }
    } else {
        return Err(anyhow!("Unsupported managed snapshot event type"));
    }
    if let Some(assignment_id) = job.assignment_id.as_deref() {
        object.insert("assignmentId".into(), json!(assignment_id));
    }
    if let Some(policy_id) = job.policy_id.as_deref() {
        object.insert("policyId".into(), json!(policy_id));
    }
    if let Some(revision) = job.policy_revision {
        object.insert("policyRevision".into(), json!(revision));
    }
    queue_event_value_tx(tx, &job.installation_id, Value::Object(object))
}

fn queue_event_value_tx(tx: &Transaction<'_>, installation_id: &str, payload: Value) -> Result<()> {
    let event_id = payload
        .get("eventId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Managed event is missing its immutable ID"))?;
    let encoded = serde_json::to_string(&payload)?;
    if encoded.len() > 8 * 1024 {
        return Err(anyhow!("Managed event exceeded its local size limit"));
    }
    tx.execute(
        "INSERT INTO managed_control_events
         (event_id, installation_id, payload, created_at, sent_at)
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![event_id, installation_id, encoded, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManagedJob> {
    Ok(ManagedJob {
        job_id: row.get(0)?,
        installation_id: row.get(1)?,
        owner_account: row.get(2)?,
        command_id: row.get(3)?,
        kind: row.get(4)?,
        trigger_kind: row.get(5)?,
        assignment_id: row.get(6)?,
        policy_id: row.get(7)?,
        policy_revision: row.get(8)?,
        policy_state_generation: row.get(9)?,
        snapshot_id: row.get(10)?,
        destination_path: row.get(11)?,
        restore_staging_path: row.get(12)?,
        cancel_requested: row.get::<_, i64>(13)? != 0,
    })
}

fn load_job(tx: &Transaction<'_>, job_id: &str) -> Result<ManagedJob> {
    tx.query_row(
        "SELECT job_id, installation_id, owner_account, command_id, kind, trigger_kind,
                assignment_id, policy_id, policy_revision, policy_state_generation, snapshot_id,
                destination_path, restore_staging_path, cancel_requested
           FROM managed_jobs WHERE job_id = ?1",
        params![job_id],
        job_from_row,
    )
    .context("Managed job disappeared")
}

fn queue_initial_job_event(tx: &Transaction<'_>, job_id: &str) -> Result<()> {
    let job = load_job(tx, job_id)?;
    queue_job_event_tx(tx, &job, "job_queued", None, None)
}

fn pending_acknowledgements(
    conn: &Connection,
    installation_id: &str,
) -> Result<Vec<OrganizationControlAcknowledgement>> {
    let mut statement = conn.prepare(
        "SELECT command_id, lease_id, ack_status, completed_at, ack_result, retryable
           FROM managed_command_receipts
          WHERE installation_id = ?1 AND ack_sent = 0
          ORDER BY completed_at, command_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![installation_id, MAX_PENDING_ACKS as i64], |row| {
        let result_json: Option<String> = row.get(4)?;
        let retryable: Option<i64> = row.get(5)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            result_json,
            retryable,
        ))
    })?;
    rows.map(|row| {
        let (command_id, lease_id, status, completed_at, result_json, retryable) = row?;
        Ok(OrganizationControlAcknowledgement {
            command_id,
            lease_id,
            status,
            completed_at,
            result: result_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            retryable: retryable.map(|value| value != 0),
        })
    })
    .collect()
}

fn mark_acknowledgements_sent(
    conn: &Connection,
    acknowledgements: &[OrganizationControlAcknowledgement],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for acknowledgement in acknowledgements {
        tx.execute(
            "UPDATE managed_command_receipts SET ack_sent = 1
              WHERE command_id = ?1 AND lease_id = ?2",
            params![acknowledgement.command_id, acknowledgement.lease_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn pending_events(conn: &Connection, installation_id: &str) -> Result<Vec<(String, Value)>> {
    let mut statement = conn.prepare(
        "SELECT event_id, payload FROM managed_control_events
          WHERE installation_id = ?1 AND sent_at IS NULL
          ORDER BY created_at, event_id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![installation_id, MAX_PENDING_EVENTS as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut events = Vec::new();
    let mut body_size = 64usize;
    for row in rows {
        let (event_id, payload) = row?;
        let value: Value = serde_json::from_str(&payload)?;
        let encoded_size = payload.len() + 1;
        if !events.is_empty() && body_size + encoded_size > MAX_EVENT_BODY_BYTES {
            break;
        }
        body_size += encoded_size;
        events.push((event_id, value));
    }
    Ok(events)
}

fn mark_events_sent(conn: &Connection, event_ids: &[String]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let now = Utc::now().to_rfc3339();
    for event_id in event_ids {
        tx.execute(
            "UPDATE managed_control_events SET sent_at = ?1
              WHERE event_id = ?2 AND sent_at IS NULL",
            params![now, event_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn current_poll_id(conn: &Connection, installation_id: &str) -> Result<String> {
    if let Some(poll_id) = conn
        .query_row(
            "SELECT poll_id FROM managed_poll_state WHERE installation_id = ?1",
            params![installation_id],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(poll_id);
    }
    let poll_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO managed_poll_state (installation_id, poll_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![installation_id, poll_id, Utc::now().to_rfc3339()],
    )?;
    Ok(poll_id)
}

fn clear_poll_id(conn: &Connection, installation_id: &str, poll_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM managed_poll_state WHERE installation_id = ?1 AND poll_id = ?2",
        params![installation_id, poll_id],
    )?;
    Ok(())
}

async fn flush_pending_acks(context: &ManagedDeviceContext, state: &AppStateWrapper) -> Result<()> {
    let acknowledgements = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        pending_acknowledgements(&guard.db, &context.installation_id)?
    };
    if acknowledgements.is_empty() {
        return Ok(());
    }
    context
        .account
        .api
        .organization_control_acknowledge(&context.device_credential, &acknowledgements)
        .await?;
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    mark_acknowledgements_sent(&guard.db, &acknowledgements)
}

async fn flush_pending_events(
    context: &ManagedDeviceContext,
    state: &AppStateWrapper,
) -> Result<()> {
    let pending = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        pending_events(&guard.db, &context.installation_id)?
    };
    if pending.is_empty() {
        return Ok(());
    }
    let values: Vec<_> = pending.iter().map(|(_, value)| value.clone()).collect();
    context
        .account
        .api
        .organization_control_events(&context.device_credential, &values)
        .await?;
    let ids: Vec<_> = pending.into_iter().map(|(id, _)| id).collect();
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    mark_events_sent(&guard.db, &ids)
}

async fn cleanup_if_device_credential_revoked(
    error: &anyhow::Error,
    context: &ManagedDeviceContext,
    state: &AppStateWrapper,
) -> Result<bool> {
    if !crate::organization_enrollment::device_credential_was_revoked(error) {
        return Ok(false);
    }
    crate::organization_enrollment::cleanup_revoked_managed_context(state, context).await?;
    Ok(true)
}

fn running_cancel_requests(conn: &Connection, installation_id: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT job_id FROM managed_jobs
          WHERE installation_id = ?1 AND status = 'running' AND cancel_requested = 1",
    )?;
    let job_ids = statement
        .query_map(params![installation_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(anyhow::Error::from)?;
    Ok(job_ids)
}

pub async fn control_tick(app: tauri::AppHandle, state: &AppStateWrapper) -> Result<()> {
    let Some(context) = crate::organization_enrollment::managed_device_context(state).await? else {
        return Ok(());
    };

    // Outcomes and acknowledgements survive offline periods and process
    // restarts. Deliver them before leasing more work, but do not prevent a
    // poll merely because event telemetry is temporarily unavailable.
    if let Err(error) = flush_pending_events(&context, state).await {
        if cleanup_if_device_credential_revoked(&error, &context, state).await? {
            return Ok(());
        }
        eprintln!("Managed-device event delivery deferred: {error}");
    }
    if let Err(error) = flush_pending_acks(&context, state).await {
        if cleanup_if_device_credential_revoked(&error, &context, state).await? {
            return Ok(());
        }
        eprintln!("Managed-device acknowledgement delivery deferred: {error}");
    }

    let poll_id = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        current_poll_id(&guard.db, &context.installation_id)?
    };
    let response = match context
        .account
        .api
        .organization_control_poll(&context.device_credential, &poll_id, CONTROL_POLL_LIMIT)
        .await
    {
        Ok(response) => response,
        Err(error) if crate::organization_enrollment::device_credential_was_revoked(&error) => {
            crate::organization_enrollment::cleanup_revoked_managed_context(state, &context)
                .await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if response.schema_version != ORGANIZATION_CONTROL_SCHEMA_VERSION
        || response.poll_id != poll_id
        || DateTime::parse_from_rfc3339(&response.lease_expires_at).is_err()
        || response.commands.len() > CONTROL_POLL_LIMIT as usize
        || response
            .commands
            .iter()
            .any(|command| !valid_command_envelope(command, Some(&poll_id)))
        || !validate_identifier(&response.organization.id)
        || response.organization.name.trim().is_empty()
        || utf16_len(response.organization.name.trim()) > 120
        || contains_json_unsafe_text(&response.organization.name)
    {
        return Err(anyhow!("Invalid organization managed-device poll response"));
    }

    let mut acknowledgements = Vec::new();
    let mut cancellation_ids = Vec::new();
    {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        for command in &response.commands {
            let (acknowledgement, cancel_job_id) =
                process_command(&guard.db, &context, &response.organization, command)?;
            acknowledgements.push(acknowledgement);
            if let Some(job_id) = cancel_job_id {
                cancellation_ids.push(job_id);
            }
        }
        cancellation_ids.extend(running_cancel_requests(
            &guard.db,
            &context.installation_id,
        )?);
    }
    cancellation_ids.sort();
    cancellation_ids.dedup();
    for job_id in cancellation_ids {
        let _ = crate::backup_operations::cancel_operation(&job_id).await;
    }

    if !acknowledgements.is_empty() {
        if let Err(error) = context
            .account
            .api
            .organization_control_acknowledge(&context.device_credential, &acknowledgements)
            .await
        {
            if cleanup_if_device_credential_revoked(&error, &context, state).await? {
                return Ok(());
            }
            return Err(error);
        }
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        mark_acknowledgements_sent(&guard.db, &acknowledgements)?;
    }
    {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        clear_poll_id(&guard.db, &context.installation_id, &poll_id)?;
        cleanup_local_history(&guard.db)?;
    }
    dispatch_pending_job(app, state, context).await?;
    Ok(())
}

fn cleanup_local_history(conn: &Connection) -> Result<()> {
    // Pending delivery is never pruned. Terminal history is retained long
    // enough for operator investigation, then bounded to stop a long-lived
    // device from growing SQLite without limit.
    conn.execute(
        "DELETE FROM managed_command_receipts
          WHERE ack_sent = 1 AND datetime(completed_at) < datetime('now', '-30 days')",
        [],
    )?;
    conn.execute(
        "DELETE FROM managed_command_receipts WHERE ack_sent = 1 AND command_id NOT IN
         (SELECT command_id FROM managed_command_receipts WHERE ack_sent = 1
          ORDER BY completed_at DESC LIMIT 2000)",
        [],
    )?;
    conn.execute(
        "DELETE FROM managed_control_events
          WHERE sent_at IS NOT NULL AND datetime(sent_at) < datetime('now', '-30 days')",
        [],
    )?;
    conn.execute(
        "DELETE FROM managed_control_events WHERE sent_at IS NOT NULL AND event_id NOT IN
         (SELECT event_id FROM managed_control_events WHERE sent_at IS NOT NULL
          ORDER BY created_at DESC LIMIT 5000)",
        [],
    )?;
    conn.execute(
        "DELETE FROM managed_jobs
          WHERE status IN ('succeeded','failed','cancelled','interrupted')
            AND datetime(completed_at) < datetime('now', '-90 days')",
        [],
    )?;
    conn.execute(
        "DELETE FROM managed_jobs
          WHERE status IN ('succeeded','failed','cancelled','interrupted') AND job_id NOT IN
         (SELECT job_id FROM managed_jobs
           WHERE status IN ('succeeded','failed','cancelled','interrupted')
           ORDER BY completed_at DESC LIMIT 2000)",
        [],
    )?;
    Ok(())
}

fn finish_queued_cancellations(tx: &Transaction<'_>, installation_id: &str) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT job_id FROM managed_jobs
          WHERE installation_id = ?1 AND status = 'queued' AND cancel_requested = 1",
    )?;
    let job_ids = statement
        .query_map(params![installation_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for job_id in job_ids {
        tx.execute(
            "UPDATE managed_jobs
                SET status = 'cancelled', completed_at = ?1, error_code = NULL
              WHERE job_id = ?2 AND status = 'queued'",
            params![Utc::now().to_rfc3339(), job_id],
        )?;
        let job = load_job(tx, &job_id)?;
        queue_job_event_tx(tx, &job, "job_cancelled", None, None)?;
    }
    Ok(())
}

pub(crate) fn disconnect_request_id(
    conn: &Connection,
    installation_id: &str,
    owner_account: &str,
) -> Result<String> {
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT owner_account, request_id FROM managed_disconnect_state
              WHERE installation_id = ?1",
            params![installation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((stored_owner, request_id)) = existing {
        if stored_owner != owner_account || !valid_uuid(&request_id) {
            return Err(anyhow!(
                "The pending organization disconnect belongs to another account context"
            ));
        }
        return Ok(request_id);
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO managed_disconnect_state
         (installation_id, owner_account, request_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            installation_id,
            owner_account,
            request_id,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(request_id)
}

pub(crate) async fn disconnect_local_managed_state(
    state: &AppStateWrapper,
    installation_id: &str,
    owner_account: &str,
) -> Result<usize> {
    let running_jobs = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        let tx = guard.db.unchecked_transaction()?;
        tx.execute(
            "UPDATE managed_backup_policies
                SET desired_state = 'removed', schedule_state = 'removed',
                    next_run = NULL, retry_count = 0, retry_at = NULL,
                    last_error_code = NULL, updated_at = ?1
              WHERE installation_id = ?2 AND owner_account = ?3",
            params![Utc::now().to_rfc3339(), installation_id, owner_account],
        )?;
        tx.execute(
            "UPDATE managed_jobs SET cancel_requested = 1
              WHERE installation_id = ?1 AND owner_account = ?2
                AND status IN ('queued','running')",
            params![installation_id, owner_account],
        )?;
        finish_queued_cancellations(&tx, installation_id)?;
        let mut statement = tx.prepare(
            "SELECT job_id, kind, snapshot_id FROM managed_jobs
              WHERE installation_id = ?1 AND owner_account = ?2 AND status = 'running'",
        )?;
        let jobs = statement
            .query_map(params![installation_id, owner_account], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        tx.execute(
            "DELETE FROM managed_disconnect_state
              WHERE installation_id = ?1 AND owner_account = ?2",
            params![installation_id, owner_account],
        )?;
        tx.execute(
            "DELETE FROM managed_poll_state WHERE installation_id = ?1",
            params![installation_id],
        )?;
        tx.commit()?;
        jobs
    };

    for (job_id, kind, snapshot_id) in &running_jobs {
        let _ = crate::backup_operations::cancel_operation(job_id).await;
        if kind == "restore" {
            if let Some(snapshot_id) = snapshot_id.as_deref() {
                crate::kopia::cancel_restore(snapshot_id);
            }
        }
    }
    Ok(running_jobs.len())
}

fn claim_next_job(
    conn: &Connection,
    installation_id: &str,
    owner_account: &str,
) -> Result<Option<ManagedJob>> {
    let tx = conn.unchecked_transaction()?;
    finish_queued_cancellations(&tx, installation_id)?;
    let running: i64 = tx.query_row(
        "SELECT COUNT(*) FROM managed_jobs
          WHERE installation_id = ?1 AND status = 'running'",
        params![installation_id],
        |row| row.get(0),
    )?;
    if running > 0 {
        tx.commit()?;
        return Ok(None);
    }
    let job_id: Option<String> = tx
        .query_row(
            "SELECT job_id FROM managed_jobs
              WHERE installation_id = ?1 AND owner_account = ?2
                AND status = 'queued' AND cancel_requested = 0
              ORDER BY created_at, job_id LIMIT 1",
            params![installation_id, owner_account],
            |row| row.get(0),
        )
        .optional()?;
    let Some(job_id) = job_id else {
        tx.commit()?;
        return Ok(None);
    };
    let changed = tx.execute(
        "UPDATE managed_jobs SET status = 'running', started_at = ?1
          WHERE job_id = ?2 AND status = 'queued' AND cancel_requested = 0",
        params![Utc::now().to_rfc3339(), job_id],
    )?;
    if changed != 1 {
        tx.commit()?;
        return Ok(None);
    }
    let job = load_job(&tx, &job_id)?;
    queue_job_event_tx(&tx, &job, "job_started", None, None)?;
    tx.commit()?;
    Ok(Some(job))
}

async fn dispatch_pending_job(
    app: tauri::AppHandle,
    state: &AppStateWrapper,
    context: ManagedDeviceContext,
) -> Result<()> {
    let job = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        claim_next_job(
            &guard.db,
            &context.installation_id,
            &context.account.account_scope,
        )?
    };
    let Some(job) = job else { return Ok(()) };
    tokio::spawn(async move {
        let state = app.state::<AppStateWrapper>();
        if let Err(error) = execute_job(&app, state.inner(), &context, job).await {
            eprintln!("Managed-device job finalization failed: {error}");
        }
    });
    Ok(())
}

fn job_cancel_requested(state: &AppStateWrapper, job_id: &str) -> Result<bool> {
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    guard
        .db
        .query_row(
            "SELECT cancel_requested FROM managed_jobs WHERE job_id = ?1",
            params![job_id],
            |row| Ok(row.get::<_, i64>(0)? != 0),
        )
        .context("Managed job disappeared")
}

fn managed_backup_job_matches_policy(job: &ManagedJob, policy: &ManagedBackupPolicy) -> bool {
    policy.desired_state == "active"
        && job.policy_id.as_deref() == Some(policy.policy_id.as_str())
        && job.policy_revision == Some(policy.revision)
        && job.policy_state_generation == Some(policy.state_generation)
}

fn queue_retention_snapshot_deleted(
    state: &AppStateWrapper,
    job: &ManagedJob,
    snapshot_id: &str,
) -> Result<()> {
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    let tx = guard.db.unchecked_transaction()?;
    queue_snapshot_event_tx(
        &tx,
        job,
        "snapshot_deleted",
        snapshot_id,
        None,
        Some("retention"),
    )?;
    tx.commit()?;
    Ok(())
}

async fn prune_managed_policy_snapshots(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    engine: &crate::kopia::EngineLease<'_>,
    operation: &crate::backup_operations::BackupOperation,
    job: &ManagedJob,
    policy: &ManagedBackupPolicy,
    managed_folder: &str,
) -> Result<Vec<String>> {
    if policy.retention <= 0 {
        return Ok(Vec::new());
    }
    operation.ensure_not_cancelled()?;
    let manifest = operation.api().get_kopia_manifest().await?;
    let snapshots: Vec<crate::kopia::KopiaSnapshot> =
        serde_json::from_value(manifest).context("Invalid kopia snapshot manifest")?;
    let expired = crate::kopia::expired_profile_snapshot_ids(
        snapshots,
        policy.profile_id(),
        managed_folder,
        policy.retention as usize,
    );
    let mut deleted = Vec::new();
    for snapshot_id in expired {
        operation.ensure_not_cancelled()?;
        crate::kopia::delete_snapshot_with_context_and_control(
            app,
            engine,
            &operation.context,
            &snapshot_id,
            Some(&operation.control),
        )
        .await?;
        queue_retention_snapshot_deleted(state, job, &snapshot_id)?;
        deleted.push(snapshot_id);
    }
    if !deleted.is_empty() {
        crate::kopia::schedule_storage_cleanup_with_context(app.clone(), operation.context.clone());
    }
    Ok(deleted)
}

#[derive(Debug)]
struct ManagedBackupSource {
    path: PathBuf,
    // Prevents the canonical source or any ancestor from being renamed into a
    // junction while Kopia opens the source by path.
    _guards: Vec<std::fs::File>,
}

fn validate_managed_backup_source(value: &str) -> Result<ManagedBackupSource> {
    if !validate_managed_source_path_syntax(value) {
        return Err(anyhow!(
            "MANAGED_SOURCE_INVALID: Managed backup sources must use a local Windows drive"
        ));
    }
    let source = PathBuf::from(value);
    if !source.is_dir() {
        return Err(anyhow!(
            "SOURCE_MISSING: The managed backup source folder is unavailable"
        ));
    }
    if !restore_destination_drive_is_local(value) || source.ancestors().any(path_is_reparse_point) {
        return Err(anyhow!(
            "MANAGED_SOURCE_REMOTE: Managed backup sources cannot use mapped drives or reparse points"
        ));
    }
    let canonical = std::fs::canonicalize(&source)
        .context("MANAGED_SOURCE_UNAVAILABLE: Failed to resolve managed backup source")?;
    let canonical_value = canonical.to_string_lossy();
    if !validate_windows_local_absolute_path(&canonical_value)
        || !restore_destination_drive_is_local(&canonical_value)
    {
        return Err(anyhow!(
            "MANAGED_SOURCE_REMOTE: Managed backup source resolved outside a local Windows drive"
        ));
    }
    let guards = open_pinned_local_ancestors(&canonical)
        .context("MANAGED_SOURCE_UNAVAILABLE: Failed to pin managed backup source")?;
    Ok(ManagedBackupSource {
        path: canonical,
        _guards: guards,
    })
}

async fn run_managed_backup(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &ManagedDeviceContext,
    job: &ManagedJob,
) -> Result<String> {
    let assignment_id = job
        .assignment_id
        .as_deref()
        .ok_or_else(|| anyhow!("Managed backup is missing its assignment"))?;
    let policy = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        load_policy(
            &guard.db,
            &context.installation_id,
            &context.account.account_scope,
            assignment_id,
        )?
        .ok_or_else(|| anyhow!("Managed backup policy is unavailable"))?
    };
    if !managed_backup_job_matches_policy(job, &policy) {
        return Err(anyhow!(
            "MANAGED_POLICY_CHANGED: The managed policy changed before backup"
        ));
    }
    let source = validate_managed_backup_source(&policy.source_path)?;

    let operation = crate::backup_operations::begin_with_context_and_id(
        state,
        context.account.clone(),
        Some(&job.job_id),
        format!("Managed backup: {}", policy.name),
    )?;
    if job_cancel_requested(state, &job.job_id)? {
        let _ = crate::backup_operations::cancel_operation(&job.job_id).await;
    }
    operation.ensure_not_cancelled()?;
    crate::kopia::prepare_repository_for_backup(app, &operation).await?;
    let engine = crate::kopia::begin_operation().await?;
    let result = async {
        let managed_folder = operation
            .api()
            .ensure_profile_folder(policy.profile_id(), &policy.name)
            .await?;
        {
            let guard = state
                .0
                .lock()
                .map_err(|error| anyhow!("Lock error: {error}"))?;
            guard.db.execute(
                "UPDATE managed_backup_policies SET folder = ?1, updated_at = ?2
                  WHERE installation_id = ?3 AND owner_account = ?4
                    AND assignment_id = ?5 AND revision = ?6 AND state_generation = ?7",
                params![
                    managed_folder,
                    Utc::now().to_rfc3339(),
                    context.installation_id,
                    context.account.account_scope,
                    policy.assignment_id,
                    policy.revision,
                    policy.state_generation,
                ],
            )?;
        }
        let _pruned_before = prune_managed_policy_snapshots(
            app,
            state,
            &engine,
            &operation,
            job,
            &policy,
            &managed_folder,
        )
        .await?;
        let snapshot_id = crate::kopia::backup_paths_with_operation(
            app,
            &engine,
            &operation,
            vec![source.path.to_string_lossy().to_string()],
            if job.trigger_kind == "managed_scheduled" {
                "managed_scheduled"
            } else {
                "managed_manual"
            },
            &managed_folder,
            Some((policy.profile_id(), &policy.name)),
            (policy.retention > 0).then_some(policy.retention as usize),
        )
        .await?;
        let _pruned_after = prune_managed_policy_snapshots(
            app,
            state,
            &engine,
            &operation,
            job,
            &policy,
            &managed_folder,
        )
        .await?;
        let _ = operation.api().enforce_retention().await;
        Ok(snapshot_id)
    }
    .await;
    operation.finish_tracking().await;
    result
}

struct ManagedRestoreStaging {
    path: PathBuf,
    parent: PathBuf,
    // On Windows this handle deliberately omits FILE_SHARE_DELETE, preventing
    // a sibling process from replacing the directory with a junction while
    // Kopia writes decrypted data into it.
    guard: Option<std::fs::File>,
    // Holding every canonical ancestor without FILE_SHARE_DELETE keeps the
    // textual staging path anchored while Kopia opens it by name. Otherwise a
    // writable ancestor could be renamed and replaced by a network junction
    // after validation.
    ancestor_guards: Vec<std::fs::File>,
    cleanup_on_drop: bool,
}

impl ManagedRestoreStaging {
    fn cleanup(mut self) -> bool {
        self.cleanup_with_probe(|_| {})
    }

    fn cleanup_with_probe<F>(&mut self, probe: F) -> bool
    where
        F: FnOnce(&Path),
    {
        drop(self.guard.take());
        self.cleanup_on_drop = false;
        // The probe seam makes the critical ordering regression-testable:
        // every ancestor guard must remain held after the staging handle is
        // closed and until recursive cleanup has completed.
        probe(&self.parent);
        let removed = remove_managed_restore_staging(&self.path, &self.parent);
        self.ancestor_guards.clear();
        removed
    }

    fn finalize(mut self, destination: &Path) -> Result<()> {
        if destination.exists() {
            return Err(anyhow!(
                "RESTORE_DESTINATION_EXISTS: Restore destination appeared during restore"
            ));
        }
        rename_open_managed_restore_staging(
            self.guard
                .as_ref()
                .ok_or_else(|| anyhow!("Restore staging handle is unavailable"))?,
            self.ancestor_guards
                .last()
                .ok_or_else(|| anyhow!("Restore parent handle is unavailable"))?,
            &self.path,
            destination,
        )?;
        self.cleanup_on_drop = false;
        Ok(())
    }
}

impl Drop for ManagedRestoreStaging {
    fn drop(&mut self) {
        drop(self.guard.take());
        if self.cleanup_on_drop {
            remove_managed_restore_staging(&self.path, &self.parent);
        }
    }
}

fn managed_restore_staging_candidate(destination: &Path, job_id: &str) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("Restore destination has no parent"))?;
    Ok(parent.join(format!(
        ".savestate-managed-restore-{job_id}-{}",
        uuid::Uuid::new_v4()
    )))
}

fn managed_restore_root() -> PathBuf {
    crate::runtime_storage::data_dir().join("SaveState Restores")
}

fn managed_restore_destination_candidate() -> PathBuf {
    managed_restore_root().join(format!("restore-{}", uuid::Uuid::new_v4()))
}

fn safe_generated_restore_leaf(value: &str) -> bool {
    value.strip_prefix("restore-").is_some_and(valid_uuid)
}

fn ensure_managed_restore_root() -> Result<ManagedBackupSource> {
    ensure_managed_restore_root_at(&crate::runtime_storage::data_dir())
}

fn ensure_managed_restore_root_at(data_dir: &Path) -> Result<ManagedBackupSource> {
    // The root is not supplied by the organization. It lives inside this
    // Windows user's environment-isolated application data, never Startup,
    // plugins, an administrator-selected folder, or a network share.
    let data = validate_managed_backup_source(&data_dir.to_string_lossy())?;
    let root = data.path.join("SaveState Restores");
    if !root.exists() {
        create_protected_managed_restore_directory(&root)?;
    }
    let pinned = validate_managed_backup_source(&root.to_string_lossy())?;
    protect_existing_managed_restore_directory(&pinned.path)?;
    if !managed_restore_staging_acl_is_protected(&pinned.path)? {
        return Err(anyhow!(
            "RESTORE_ROOT_ACL_FAILED: Restore root is not protected"
        ));
    }
    Ok(pinned)
}

fn validate_generated_managed_restore_destination(value: &str, root: &Path) -> Result<PathBuf> {
    let saved = PathBuf::from(value);
    let leaf = saved
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| safe_generated_restore_leaf(name))
        .ok_or_else(|| anyhow!("RESTORE_DESTINATION_UNSAFE: Invalid generated restore leaf"))?;
    if saved.parent() != Some(managed_restore_root().as_path()) && saved.parent() != Some(root) {
        return Err(anyhow!(
            "RESTORE_DESTINATION_UNSAFE: Restore destination is outside SaveState Restores"
        ));
    }
    Ok(root.join(leaf))
}

#[cfg(target_os = "windows")]
fn open_managed_restore_staging(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
    const FILE_SHARE_READ: u32 = 0x00000001;
    const FILE_SHARE_WRITE: u32 = 0x00000002;
    const DELETE_ACCESS: u32 = 0x00010000;

    let handle = std::fs::OpenOptions::new()
        .access_mode(DELETE_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .context("Failed to hold the managed restore staging directory")?;
    if handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(anyhow!(
            "RESTORE_STAGING_REPARSE_POINT: Restore staging cannot be a reparse point"
        ));
    }
    Ok(handle)
}

#[cfg(target_os = "windows")]
fn open_pinned_local_directory(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
    const FILE_SHARE_READ: u32 = 0x00000001;
    const FILE_SHARE_WRITE: u32 = 0x00000002;
    const GENERIC_READ_ACCESS: u32 = 0x80000000;

    let handle = std::fs::OpenOptions::new()
        // Desired access 0 is a metadata-only handle for which Windows may
        // ignore sharing restrictions. GENERIC_READ makes omitted
        // FILE_SHARE_DELETE enforce directory/ancestor rename exclusion.
        .access_mode(GENERIC_READ_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .with_context(|| format!("Failed to pin restore ancestor {}", path.display()))?;
    if handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(anyhow!(
            "LOCAL_PATH_REPARSE_POINT: Pinned local ancestry cannot contain a reparse point"
        ));
    }
    Ok(handle)
}

#[cfg(target_os = "windows")]
fn open_pinned_local_ancestors(parent: &Path) -> Result<Vec<std::fs::File>> {
    let mut ancestors: Vec<PathBuf> = parent
        .ancestors()
        .filter(|candidate| candidate.is_dir())
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();
    let mut guards = Vec::with_capacity(ancestors.len());
    for ancestor in ancestors {
        guards.push(open_pinned_local_directory(&ancestor)?);
    }
    if guards.is_empty()
        || std::fs::canonicalize(parent)
            .map(|resolved| resolved != parent)
            .unwrap_or(true)
    {
        return Err(anyhow!(
            "LOCAL_PATH_CHANGED: Local path changed while it was being pinned"
        ));
    }
    Ok(guards)
}

#[cfg(not(target_os = "windows"))]
fn open_pinned_local_ancestors(_parent: &Path) -> Result<Vec<std::fs::File>> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn rename_open_managed_restore_staging(
    guard: &std::fs::File,
    _parent_guard: &std::fs::File,
    _source: &Path,
    destination: &Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO,
    };

    let destination_value = destination.to_string_lossy();
    let destination_value = destination_value
        .strip_prefix(r"\\?\")
        .unwrap_or(&destination_value);
    let mut destination_wide: Vec<u16> = std::ffi::OsStr::new(destination_value)
        .encode_wide()
        .collect();
    let file_name_bytes = destination_wide.len() * std::mem::size_of::<u16>();
    // Explicitly NUL-terminate FILE_RENAME_INFO's FileName per its string
    // contract while the byte length excludes the terminator. Without this
    // the native acceptance test observed Windows appending
    // adjacent memory as random UTF-16 characters to a perfectly valid leaf.
    destination_wide.push(0);
    let file_name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let byte_len = file_name_offset + destination_wide.len() * std::mem::size_of::<u16>();
    let word_len = byte_len.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; word_len];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` is aligned for FILE_RENAME_INFO and sized for its
    // variable UTF-16 filename tail. The handle stays valid for the call.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = file_name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            destination_wide.as_ptr(),
            buffer
                .as_mut_ptr()
                .cast::<u8>()
                .add(file_name_offset)
                .cast::<u16>(),
            destination_wide.len(),
        );
        if SetFileInformationByHandle(
            guard.as_raw_handle(),
            FileRenameInfo,
            info.cast(),
            byte_len as u32,
        ) == 0
        {
            return Err(anyhow!(
                "RESTORE_FINALIZE_FAILED: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn rename_open_managed_restore_staging(
    _guard: &std::fs::File,
    _parent_guard: &std::fs::File,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    std::fs::rename(source, destination).context("RESTORE_FINALIZE_FAILED")
}

#[cfg(target_os = "windows")]
fn managed_restore_security_descriptor(
) -> Result<windows_sys::Win32::Security::PSECURITY_DESCRIPTOR> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(anyhow!(
            "RESTORE_ROOT_ACL_FAILED: Could not inspect process identity"
        ));
    }
    let identity = (|| -> Result<String> {
        let mut bytes = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut bytes);
        }
        if bytes == 0 || bytes > 64 * 1024 {
            return Err(anyhow!(
                "RESTORE_ROOT_ACL_FAILED: Invalid process identity size"
            ));
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(anyhow!(
                "RESTORE_ROOT_ACL_FAILED: Could not read process identity"
            ));
        }
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(anyhow!(
                "RESTORE_ROOT_ACL_FAILED: Could not encode process identity"
            ));
        }
        let mut length = 0;
        while unsafe { *sid.add(length) } != 0 {
            length += 1;
        }
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) });
        unsafe {
            LocalFree(sid.cast());
        }
        value.context("RESTORE_ROOT_ACL_FAILED: Invalid process identity")
    })();
    unsafe {
        CloseHandle(token);
    }
    let identity = identity?;

    // An explicit process-user owner and protected DACL grant only that
    // Windows user, LocalSystem, and built-in administrators full
    // control. OI/CI carries the restriction to every restored descendant,
    // preventing another principal with access to a shared parent from
    // planting a junction inside the plaintext staging tree.
    let mut sddl: Vec<u16> =
        format!("O:{identity}D:P(A;OICI;FA;;;{identity})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
            .encode_utf16()
            .collect();
    sddl.push(0);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the duration of the calls and the
    // converted descriptor is released using LocalFree as required by Win32.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(anyhow!(
            "RESTORE_STAGING_ACL_FAILED: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(descriptor)
}

#[cfg(target_os = "windows")]
fn create_protected_managed_restore_directory(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let descriptor = managed_restore_security_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    path_wide.push(0);
    let created = unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) };
    unsafe {
        LocalFree(descriptor);
    }
    if created == 0 {
        return Err(anyhow!(
            "RESTORE_STAGING_CREATE_FAILED: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn protect_existing_managed_restore_directory(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let descriptor = managed_restore_security_descriptor()?;
    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    path_wide.push(0);
    let secured = unsafe {
        SetFileSecurityW(
            path_wide.as_ptr(),
            DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    if secured == 0 {
        return Err(anyhow!(
            "RESTORE_ROOT_ACL_FAILED: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn protect_existing_managed_restore_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn managed_restore_staging_acl_is_protected(path: &Path) -> Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorControl, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED,
    };

    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    path_wide.push(0);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 || descriptor.is_null() {
        return Err(anyhow!(
            "RESTORE_STAGING_ACL_FAILED: Could not inspect protected ACL ({status})"
        ));
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    let inspected =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    unsafe {
        LocalFree(descriptor);
    }
    if inspected == 0 {
        return Err(anyhow!(
            "RESTORE_STAGING_ACL_FAILED: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

#[cfg(not(target_os = "windows"))]
fn managed_restore_staging_acl_is_protected(_path: &Path) -> Result<bool> {
    Ok(true)
}

#[cfg(not(target_os = "windows"))]
fn create_protected_managed_restore_directory(path: &Path) -> Result<()> {
    std::fs::create_dir(path)
        .context("RESTORE_STAGING_CREATE_FAILED: Failed to reserve restore staging")
}

#[cfg(not(target_os = "windows"))]
fn open_managed_restore_staging(path: &Path) -> Result<std::fs::File> {
    std::fs::File::open(path).context("Failed to hold the managed restore staging directory")
}

#[cfg(target_os = "windows")]
fn path_is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .map(|metadata| classified_file_attributes_is_reparse(metadata.file_attributes()))
        .unwrap_or(true)
}

#[cfg(target_os = "windows")]
fn classified_file_attributes_is_reparse(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(target_os = "windows"))]
fn path_is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(true)
}

fn validate_managed_restore_staging(staging: &Path, destination_parent: &Path) -> Result<()> {
    let is_owned_staging = staging.parent() == Some(destination_parent)
        && staging
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".savestate-managed-restore-"));
    if !is_owned_staging || !staging.is_dir() || path_is_reparse_point(staging) {
        return Err(anyhow!(
            "RESTORE_STAGING_UNSAFE: Restore staging is outside the reserved local directory"
        ));
    }
    let canonical_staging = std::fs::canonicalize(staging)
        .context("Failed to canonicalize the managed restore staging directory")?;
    if canonical_staging.parent() != Some(destination_parent)
        || !validate_windows_local_absolute_path(&canonical_staging.to_string_lossy())
        || !restore_destination_drive_is_local(&canonical_staging.to_string_lossy())
    {
        return Err(anyhow!(
            "RESTORE_STAGING_REMOTE: Restore staging resolved outside the canonical local parent"
        ));
    }
    Ok(())
}

fn create_managed_restore_staging(
    staging: &Path,
    destination_parent: &Path,
) -> Result<ManagedRestoreStaging> {
    let ancestor_guards = open_pinned_local_ancestors(destination_parent)?;
    create_protected_managed_restore_directory(staging)?;
    match managed_restore_staging_acl_is_protected(staging) {
        Ok(true) => {}
        Ok(false) => {
            let _ = std::fs::remove_dir(staging);
            return Err(anyhow!(
                "RESTORE_STAGING_ACL_FAILED: Restore staging inherited an unsafe ACL"
            ));
        }
        Err(error) => {
            let _ = std::fs::remove_dir(staging);
            return Err(error);
        }
    }
    let guard = match open_managed_restore_staging(staging) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = std::fs::remove_dir(staging);
            return Err(error);
        }
    };
    if let Err(error) = validate_managed_restore_staging(staging, destination_parent) {
        drop(guard);
        let _ = std::fs::remove_dir(staging);
        return Err(error);
    }
    Ok(ManagedRestoreStaging {
        path: staging.to_path_buf(),
        parent: destination_parent.to_path_buf(),
        guard: Some(guard),
        ancestor_guards,
        cleanup_on_drop: true,
    })
}

fn remove_managed_restore_staging(staging: &Path, destination_parent: &Path) -> bool {
    if !staging.exists() {
        return true;
    }
    let _ancestor_guards = match open_pinned_local_ancestors(destination_parent) {
        Ok(guards) => guards,
        Err(_) => return false,
    };
    if validate_managed_restore_staging(staging, destination_parent).is_err() {
        // Never recurse through a path that was replaced by a junction or
        // otherwise escaped the canonical local parent.
        return false;
    }
    std::fs::remove_dir_all(staging).is_ok()
}

fn reserve_restore_staging_path(
    state: &AppStateWrapper,
    job: &ManagedJob,
    destination: &Path,
) -> Result<PathBuf> {
    let staging = managed_restore_staging_candidate(destination, &job.job_id)?;
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    let changed = guard.db.execute(
        "UPDATE managed_jobs SET restore_staging_path = ?1, destination_path = ?3
          WHERE job_id = ?2 AND status = 'running' AND restore_staging_path IS NULL",
        params![
            staging.to_string_lossy(),
            job.job_id,
            destination.to_string_lossy()
        ],
    )?;
    if changed != 1 {
        return Err(anyhow!(
            "RESTORE_STAGING_STATE_CHANGED: Restore job no longer owns staging"
        ));
    }
    Ok(staging)
}

fn clear_restore_staging_path(state: &AppStateWrapper, job_id: &str) -> Result<()> {
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    guard.db.execute(
        "UPDATE managed_jobs SET restore_staging_path = NULL WHERE job_id = ?1",
        params![job_id],
    )?;
    Ok(())
}

async fn run_managed_restore(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &AccountContext,
    job: &ManagedJob,
) -> Result<String> {
    let snapshot_id = job
        .snapshot_id
        .as_deref()
        .ok_or_else(|| anyhow!("Managed restore is missing its snapshot"))?;
    let destination_value = job
        .destination_path
        .as_deref()
        .ok_or_else(|| anyhow!("Managed restore is missing its destination"))?;
    if job.restore_staging_path.is_some() {
        return Err(anyhow!(
            "RESTORE_STAGING_STATE_CHANGED: Restore job already owns staging"
        ));
    }
    let restore_root = ensure_managed_restore_root()?;
    let generated =
        validate_generated_managed_restore_destination(destination_value, &restore_root.path)?;
    let destination = validate_restore_destination(&generated.to_string_lossy())
        .map_err(|failure| anyhow!(failure.result.to_string()))?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("Managed restore destination has no parent"))?;
    let staging_path = reserve_restore_staging_path(state, job, &destination)?;
    let staging = create_managed_restore_staging(&staging_path, parent)?;
    let operation = crate::backup_operations::begin_with_context_and_id(
        state,
        context.clone(),
        Some(&job.job_id),
        "Managed snapshot restore",
    )?;
    if job_cancel_requested(state, &job.job_id)? {
        let _ = crate::backup_operations::cancel_operation(&job.job_id).await;
        crate::kopia::cancel_restore(snapshot_id);
    }
    operation.ensure_not_cancelled()?;
    let engine = crate::kopia::begin_operation().await?;
    validate_managed_restore_staging(&staging.path, parent)?;
    let result = crate::kopia::restore_snapshot_with_context_and_control(
        app,
        &engine,
        context,
        snapshot_id,
        &staging.path.to_string_lossy(),
        "managed_restore",
        Some(&operation.control),
    )
    .await;
    operation.finish_tracking().await;
    match result {
        Ok(()) => {
            validate_managed_restore_staging(&staging.path, parent)?;
            if destination.exists() {
                return Err(anyhow!(
                    "RESTORE_DESTINATION_EXISTS: Restore destination appeared during restore"
                ));
            }
            staging.finalize(&destination)?;
            if let Err(error) = clear_restore_staging_path(state, &job.job_id) {
                eprintln!("Failed to clear completed managed restore staging state: {error}");
            }
            Ok(snapshot_id.to_string())
        }
        Err(error) => {
            if staging.cleanup() {
                if let Err(clear_error) = clear_restore_staging_path(state, &job.job_id) {
                    eprintln!("Failed to clear managed restore staging state: {clear_error}");
                }
            }
            Err(error)
        }
    }
}

async fn run_managed_snapshot_delete(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &AccountContext,
    job: &ManagedJob,
) -> Result<String> {
    let snapshot_id = job
        .snapshot_id
        .as_deref()
        .ok_or_else(|| anyhow!("Managed deletion is missing its snapshot"))?;
    context.ensure_current(app.state::<AppStateWrapper>().inner())?;
    let operation = crate::backup_operations::begin_with_context_and_id(
        state,
        context.clone(),
        Some(&job.job_id),
        "Managed snapshot deletion",
    )?;
    if job_cancel_requested(state, &job.job_id)? {
        let _ = crate::backup_operations::cancel_operation(&job.job_id).await;
    }
    operation.ensure_not_cancelled()?;
    let snapshots = crate::kopia::list_snapshots_from_repository_with_control(
        app,
        context,
        Some(&operation.control),
    )
    .await?;
    if exact_snapshot(&snapshots, snapshot_id).is_none() {
        return Err(anyhow!(
            "SNAPSHOT_NOT_FOUND: The exact snapshot is not present in this repository"
        ));
    }
    operation.ensure_not_cancelled()?;
    let engine = crate::kopia::begin_operation().await?;
    let result = crate::kopia::delete_snapshot_with_context_and_control(
        app,
        &engine,
        context,
        snapshot_id,
        Some(&operation.control),
    )
    .await;
    operation.finish_tracking().await;
    result?;
    crate::kopia::schedule_storage_cleanup(app.clone());
    Ok(snapshot_id.to_string())
}

fn sanitized_job_error(kind: &str, error: &anyhow::Error) -> &'static str {
    if crate::backup_operations::is_cancelled(error) {
        return "cancelled";
    }
    let value = format!("{error:#}").to_ascii_lowercase();
    if value.contains("managed_policy_changed") {
        return "policy_revision_conflict";
    }
    if value.contains("source_missing") || value.contains("source path does not exist") {
        return "source_missing";
    }
    if value.contains("managed_source_") {
        return "unsafe_source_path";
    }
    if value.contains("snapshot_not_found") || value.contains("snapshot not found") {
        return "snapshot_not_found";
    }
    if value.contains("destination") || value.contains("restore_staging") {
        return "restore_destination_unavailable";
    }
    if value.contains("unauthorized") || value.contains("authentication") {
        return "authentication_required";
    }
    match kind {
        "backup" => crate::scheduler::classify_schedule_failure(&value).code,
        "restore" => "restore_failed",
        "delete_snapshot" => "snapshot_delete_failed",
        _ => "managed_job_failed",
    }
}

fn update_policy_after_job(
    tx: &Transaction<'_>,
    job: &ManagedJob,
    status: &str,
    error_code: Option<&str>,
) -> Result<()> {
    let Some(assignment_id) = job.assignment_id.as_deref() else {
        return Ok(());
    };
    let Some(policy_revision) = job.policy_revision else {
        return Ok(());
    };
    let Some(policy_state_generation) = job.policy_state_generation else {
        return Ok(());
    };
    let policy = tx
        .query_row(
            "SELECT schedule, desired_state, retry_count
               FROM managed_backup_policies
              WHERE installation_id = ?1 AND owner_account = ?2
                AND assignment_id = ?3 AND revision = ?4 AND state_generation = ?5",
            params![
                job.installation_id,
                job.owner_account,
                assignment_id,
                policy_revision,
                policy_state_generation,
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((schedule, desired_state, retry_count)) = policy else {
        return Ok(());
    };
    let active = desired_state == "active";
    let next_regular = active
        .then(|| {
            schedule
                .as_deref()
                .and_then(|value| crate::profiles::compute_next_run(Some(value)))
        })
        .flatten();
    let now = Utc::now().to_rfc3339();
    if status == "succeeded" {
        tx.execute(
            "UPDATE managed_backup_policies
                SET last_run = ?1, next_run = ?2, retry_count = 0, retry_at = NULL,
                    last_error_code = NULL, schedule_state = ?3, updated_at = ?1
              WHERE installation_id = ?4 AND owner_account = ?5
                AND assignment_id = ?6 AND revision = ?7 AND state_generation = ?8",
            params![
                now,
                next_regular,
                if active { "scheduled" } else { "paused" },
                job.installation_id,
                job.owner_account,
                assignment_id,
                policy_revision,
                policy_state_generation,
            ],
        )?;
    } else if job.trigger_kind == "managed_scheduled" && active {
        let next_retry = retry_count.saturating_add(1);
        let classification = error_code.unwrap_or("temporary_operation_failure");
        let retryable = classification == "temporary_operation_failure";
        if retryable {
            if let Some(delay) = crate::scheduler::retry_delay(assignment_id, next_retry) {
                let retry_at = (Utc::now() + delay).to_rfc3339();
                tx.execute(
                    "UPDATE managed_backup_policies
                        SET retry_count = ?1, retry_at = ?2, last_error_code = ?3,
                            schedule_state = 'retrying', updated_at = ?4
                      WHERE installation_id = ?5 AND owner_account = ?6
                        AND assignment_id = ?7 AND revision = ?8 AND state_generation = ?9",
                    params![
                        next_retry,
                        retry_at,
                        classification,
                        now,
                        job.installation_id,
                        job.owner_account,
                        assignment_id,
                        policy_revision,
                        policy_state_generation,
                    ],
                )?;
                return Ok(());
            }
        }
        tx.execute(
            "UPDATE managed_backup_policies
                SET next_run = ?1, retry_at = NULL, last_error_code = ?2,
                    schedule_state = 'needs_attention', updated_at = ?3
              WHERE installation_id = ?4 AND owner_account = ?5
                AND assignment_id = ?6 AND revision = ?7 AND state_generation = ?8",
            params![
                next_regular,
                classification,
                now,
                job.installation_id,
                job.owner_account,
                assignment_id,
                policy_revision,
                policy_state_generation,
            ],
        )?;
    } else {
        tx.execute(
            "UPDATE managed_backup_policies SET last_error_code = ?1, updated_at = ?2
              WHERE installation_id = ?3 AND owner_account = ?4
                AND assignment_id = ?5 AND revision = ?6 AND state_generation = ?7",
            params![
                error_code,
                now,
                job.installation_id,
                job.owner_account,
                assignment_id,
                policy_revision,
                policy_state_generation,
            ],
        )?;
    }
    Ok(())
}

fn finish_job(
    state: &AppStateWrapper,
    job: &ManagedJob,
    status: &str,
    snapshot_id: Option<&str>,
    snapshot: Option<&crate::kopia::KopiaSnapshot>,
    error_code: Option<&str>,
) -> Result<()> {
    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    let tx = guard.db.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE managed_jobs
            SET status = ?1, completed_at = ?2, result_snapshot_id = ?3, error_code = ?4
          WHERE job_id = ?5 AND status = 'running'",
        params![
            status,
            Utc::now().to_rfc3339(),
            snapshot_id,
            error_code,
            job.job_id
        ],
    )?;
    if changed == 1 {
        let event_type = match status {
            "succeeded" => "job_succeeded",
            "cancelled" => "job_cancelled",
            _ => "job_failed",
        };
        queue_job_event_tx(
            &tx,
            job,
            event_type,
            snapshot_id,
            (event_type == "job_failed").then_some(error_code.unwrap_or("managed_job_failed")),
        )?;
        if status == "succeeded" {
            if job.kind == "backup" {
                if let Some(snapshot_id) = snapshot_id {
                    queue_snapshot_event_tx(
                        &tx,
                        job,
                        "snapshot_available",
                        snapshot_id,
                        snapshot,
                        None,
                    )?;
                }
            } else if job.kind == "delete_snapshot" {
                if let Some(snapshot_id) = snapshot_id {
                    queue_snapshot_event_tx(
                        &tx,
                        job,
                        "snapshot_deleted",
                        snapshot_id,
                        None,
                        Some("administrator"),
                    )?;
                }
            }
        }
        update_policy_after_job(&tx, job, status, error_code)?;
    }
    tx.commit()?;
    Ok(())
}

async fn execute_job(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &ManagedDeviceContext,
    job: ManagedJob,
) -> Result<()> {
    if job.cancel_requested || job_cancel_requested(state, &job.job_id)? {
        return finish_job(state, &job, "cancelled", None, None, None);
    }
    let result = match job.kind.as_str() {
        "backup" => run_managed_backup(app, state, context, &job).await,
        "restore" => run_managed_restore(app, state, &context.account, &job).await,
        "delete_snapshot" => run_managed_snapshot_delete(app, state, &context.account, &job).await,
        _ => Err(anyhow!("Unsupported managed job kind")),
    };
    match result {
        Ok(snapshot_id) => {
            let snapshot = if job.kind == "backup" {
                context
                    .account
                    .api
                    .get_kopia_manifest()
                    .await
                    .ok()
                    .and_then(|manifest| {
                        serde_json::from_value::<Vec<crate::kopia::KopiaSnapshot>>(manifest).ok()
                    })
                    .and_then(|snapshots| exact_snapshot(&snapshots, &snapshot_id).cloned())
            } else {
                None
            };
            finish_job(
                state,
                &job,
                "succeeded",
                Some(&snapshot_id),
                snapshot.as_ref(),
                None,
            )
        }
        Err(error) if crate::backup_operations::is_cancelled(&error) => {
            finish_job(state, &job, "cancelled", None, None, None)
        }
        Err(error) => {
            let error_code = sanitized_job_error(&job.kind, &error);
            finish_job(state, &job, "failed", None, None, Some(error_code))
        }
    }
}

pub fn reconcile_startup(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let restore_staging = {
        let mut statement = tx.prepare(
            "SELECT job_id, destination_path, restore_staging_path FROM managed_jobs
              WHERE kind = 'restore' AND restore_staging_path IS NOT NULL",
        )?;
        let paths = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        paths
    };
    let mut statement = tx.prepare(
        "SELECT job_id FROM managed_jobs WHERE status = 'running' ORDER BY started_at, job_id",
    )?;
    let job_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for job_id in job_ids {
        let job = load_job(&tx, &job_id)?;
        tx.execute(
            "UPDATE managed_jobs
                SET status = 'interrupted', completed_at = ?1, error_code = 'device_restarted'
              WHERE job_id = ?2 AND status = 'running'",
            params![Utc::now().to_rfc3339(), job_id],
        )?;
        queue_job_event_tx(&tx, &job, "job_failed", None, Some("device_restarted"))?;
        update_policy_after_job(&tx, &job, "failed", Some("device_restarted"))?;
    }
    let installations = {
        let mut statement = tx.prepare(
            "SELECT DISTINCT installation_id FROM managed_jobs
              WHERE status = 'queued' AND cancel_requested = 1",
        )?;
        let installations = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        installations
    };
    for installation_id in installations {
        finish_queued_cancellations(&tx, &installation_id)?;
    }
    tx.commit()?;
    for (job_id, destination, staging) in restore_staging {
        let Ok(root) = ensure_managed_restore_root() else {
            continue;
        };
        let Ok(destination) =
            validate_generated_managed_restore_destination(&destination, &root.path)
        else {
            continue;
        };
        let staging = PathBuf::from(staging);
        if let Some(parent) = destination.parent() {
            if remove_managed_restore_staging(&staging, parent) {
                conn.execute(
                    "UPDATE managed_jobs SET restore_staging_path = NULL WHERE job_id = ?1",
                    params![job_id],
                )?;
            }
        }
    }
    Ok(())
}

fn enqueue_due_schedules(
    conn: &Connection,
    context: &ManagedDeviceContext,
    now: DateTime<Utc>,
) -> Result<usize> {
    let policies = list_policies(
        conn,
        &context.installation_id,
        &context.account.account_scope,
    )?;
    let tx = conn.unchecked_transaction()?;
    let mut queued = 0;
    for policy in policies {
        if !crate::scheduler::scheduled_deadline_is_due(
            policy.enabled(),
            &policy.schedule_state,
            policy.next_run.as_deref(),
            policy.retry_at.as_deref(),
            now,
        ) {
            continue;
        }
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM managed_jobs
              WHERE installation_id = ?1 AND assignment_id = ?2
                AND status IN ('queued','running')",
            params![context.installation_id, policy.assignment_id],
            |row| row.get(0),
        )?;
        if active > 0 {
            continue;
        }
        if policy.schedule_state == "needs_attention" {
            tx.execute(
                "UPDATE managed_backup_policies
                    SET retry_count = 0, retry_at = NULL, schedule_state = 'scheduled'
                  WHERE installation_id = ?1 AND assignment_id = ?2 AND revision = ?3",
                params![
                    context.installation_id,
                    policy.assignment_id,
                    policy.revision
                ],
            )?;
        }
        insert_job(
            &tx,
            context,
            None,
            "backup",
            "managed_scheduled",
            Some(&policy.assignment_id),
            Some(&policy.policy_id),
            Some(policy.revision),
            Some(policy.state_generation),
            None,
            None,
        )
        .map_err(|failure| anyhow!(failure.result.to_string()))?;
        queued += 1;
    }
    tx.commit()?;
    Ok(queued)
}

pub async fn schedule_tick(app: tauri::AppHandle, state: &AppStateWrapper) -> Result<()> {
    let Some(context) = crate::organization_enrollment::managed_device_context(state).await? else {
        return Ok(());
    };
    {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        enqueue_due_schedules(&guard.db, &context, Utc::now())?;
    }
    dispatch_pending_job(app, state, context).await
}

#[cfg(all(test, target_os = "windows"))]
#[path = "managed_device_integration_tests.rs"]
mod managed_device_integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SaveStateClient;
    use chrono::Duration;
    use std::io::{Read, Write};

    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        conn
    }

    fn fixture_context(account_scope: &str) -> ManagedDeviceContext {
        ManagedDeviceContext {
            installation_id: "installation-one".into(),
            device_credential: "secret-never-serialized".into(),
            account: AccountContext {
                api: SaveStateClient::new("desktop-fixture".into()),
                account_scope: account_scope.into(),
                repository_password: "00".repeat(32),
                session_generation: 7,
            },
        }
    }

    fn fixture_organization() -> OrganizationControlOrganization {
        OrganizationControlOrganization {
            id: "org_one".into(),
            name: "Example IT".into(),
        }
    }

    fn one_shot_http_error(status: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 16 * 1024];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), handle)
    }

    fn fixture_command(
        id: &str,
        lease_id: &str,
        kind: &str,
        payload: Value,
    ) -> OrganizationControlCommand {
        OrganizationControlCommand {
            id: id.into(),
            kind: kind.into(),
            created_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + Duration::minutes(10)).to_rfc3339(),
            attempt: 1,
            max_attempts: 5,
            lease_id: lease_id.into(),
            payload,
        }
    }

    fn policy_payload(desired_state: &str, revision: i64) -> Value {
        json!({
            "schemaVersion": 1,
            "operation": "upsert",
            "assignmentId": "mpa_example",
            "policyId": "mbp_example",
            "revision": revision,
            "stateGeneration": revision,
            "desiredState": desired_state,
            "definition": {
                "schemaVersion": 1,
                "kind": "folder",
                "name": "Customer documents",
                "description": "Managed customer data",
                "sourcePath": "C:\\CustomerData",
                "schedule": { "times": ["02:00", "22:30"], "intervalDays": 1 },
                "retentionSnapshots": 14
            }
        })
    }

    fn result_code(ack: &OrganizationControlAcknowledgement) -> Option<&str> {
        ack.result
            .as_ref()
            .and_then(|result| result.get("code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn managed_source_and_restore_contracts_accept_only_local_drive_paths() {
        for path in [r"C:\", r"D:\Data\Customers", r"\\?\C:\VeryLong\Data"] {
            assert!(
                validate_managed_source_path_syntax(path),
                "expected accepted: {path}"
            );
        }
        for path in [
            r"\\fileserver\accounts\Backups",
            r"\\?\UNC\fileserver\share\Data",
            r"Data\Customers",
            r".\Data",
            "C:/Data",
            r"C:\Data\..\Secrets",
            r"\\fileserver",
            r"\\.\PhysicalDrive0",
            r"C:\Data\bad?.txt",
            r"C:\Data\\nested",
            r"C:\Data\bad<name>",
        ] {
            assert!(
                !validate_managed_source_path_syntax(path),
                "expected rejected: {path}"
            );
        }
        assert!(!validate_managed_source_path_syntax(
            "C:\\Data\u{0000}private"
        ));

        assert!(validate_windows_local_absolute_path(r"C:\Restore"));
        assert!(validate_windows_local_absolute_path(r"\\?\C:\Restore"));
        for path in [
            r"\\server\share\Restore",
            r"\\?\UNC\server\share\Restore",
            r"\\.\C:\Restore",
            r"C:\Data\..\Restore",
        ] {
            assert!(
                !validate_windows_local_absolute_path(path),
                "restore path must be rejected: {path}"
            );
        }
    }

    #[test]
    fn mapped_remote_drive_classification_is_rejected() {
        const DRIVE_FIXED_FIXTURE: u32 = 3;
        const DRIVE_REMOTE_FIXTURE: u32 = 4;
        const DRIVE_REMOVABLE_FIXTURE: u32 = 2;
        assert!(classified_drive_type_is_local(DRIVE_FIXED_FIXTURE));
        assert!(classified_drive_type_is_local(DRIVE_REMOVABLE_FIXTURE));
        assert!(!classified_drive_type_is_local(DRIVE_REMOTE_FIXTURE));
        assert!(!classified_drive_type_is_local(0));
        assert!(!classified_drive_type_is_local(5));
        assert!(classified_file_attributes_is_reparse(0x00000400));
        assert!(!classified_file_attributes_is_reparse(0x00000020));
        // `std::fs::canonicalize` returns extended paths on Windows. A local
        // resolved parent remains valid, while a junction resolving to a UNC
        // target is rejected before staging is created.
        assert!(validate_windows_local_absolute_path(
            r"\\?\C:\Resolved\Parent"
        ));
        assert!(!validate_windows_local_absolute_path(
            r"\\?\UNC\server\share\Resolved"
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn restore_destination_must_be_new_with_an_existing_local_parent() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("new-restore-folder");
        let destination_value = destination.to_string_lossy().to_string();
        let validated = validate_restore_destination(&destination_value).unwrap();
        let canonical_parent = std::fs::canonicalize(parent.path()).unwrap();
        assert_eq!(validated.file_name(), destination.file_name());
        assert_eq!(validated.parent(), Some(canonical_parent.as_path()));
        std::fs::create_dir(&destination).unwrap();
        assert_eq!(
            validate_restore_destination(&destination_value)
                .unwrap_err()
                .result["code"],
            "restore_destination_already_exists"
        );
        let missing_parent = parent.path().join("missing").join("destination");
        assert_eq!(
            validate_restore_destination(&missing_parent.to_string_lossy())
                .unwrap_err()
                .result["code"],
            "restore_destination_parent_missing"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn managed_source_execution_rejects_reparse_ancestors_when_supported() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let pinned = validate_managed_backup_source(&source.to_string_lossy()).unwrap();
        assert!(std::fs::rename(&source, root.path().join("source-swap")).is_err());
        drop(pinned);

        let linked = root.path().join("linked-source");
        match std::os::windows::fs::symlink_dir(&source, &linked) {
            Ok(()) => {
                let error = validate_managed_backup_source(&linked.to_string_lossy())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("MANAGED_SOURCE_REMOTE"));
            }
            Err(error) => {
                // Windows without Developer Mode or SeCreateSymbolicLinkPrivilege
                // cannot create the fixture. The deterministic attribute seam
                // is still covered by mapped_remote_drive_classification_is_rejected.
                eprintln!("Skipping live reparse fixture: {error}");
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn managed_restore_staging_is_random_protected_pinned_and_atomically_finalized() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("restored-snapshot");
        let destination = validate_restore_destination(&destination.to_string_lossy()).unwrap();
        let parent = destination.parent().unwrap();
        let first = managed_restore_staging_candidate(&destination, "job-fixture").unwrap();
        let second = managed_restore_staging_candidate(&destination, "job-fixture").unwrap();
        assert_ne!(first, second);

        let staging = create_managed_restore_staging(&first, parent).unwrap();
        assert!(managed_restore_staging_acl_is_protected(&first).unwrap());
        validate_managed_restore_staging(&first, parent).unwrap();
        std::fs::create_dir(first.join("restored-child")).unwrap();

        // The staging root and its canonical parent ancestry cannot be swapped
        // while Kopia would be opening descendants by path.
        assert!(std::fs::rename(&first, parent.join("attacker-swap")).is_err());
        let parent_swap = parent.parent().unwrap().join("attacker-parent-swap");
        assert!(std::fs::rename(parent, &parent_swap).is_err());

        staging.finalize(&destination).unwrap();
        assert!(!first.exists());
        assert!(destination.join("restored-child").is_dir());

        let cleanup_destination = parent.join("unused-destination");
        let cleanup_path =
            managed_restore_staging_candidate(&cleanup_destination, "job-cleanup").unwrap();
        let mut cleanup_staging = create_managed_restore_staging(&cleanup_path, parent).unwrap();
        std::fs::create_dir(cleanup_path.join("partial-child")).unwrap();
        assert!(cleanup_staging.cleanup_with_probe(|pinned_parent| {
            let replacement = pinned_parent.parent().unwrap().join("cleanup-parent-swap");
            assert!(std::fs::rename(pinned_parent, replacement).is_err());
        }));
        assert!(!cleanup_path.exists());
    }

    #[test]
    fn entity_ids_accept_prefixed_contract_ids_but_protocol_ids_remain_uuids() {
        assert!(validate_identifier("mbp_customer:policy-1.2"));
        assert!(validate_identifier("mpa_assignment_123"));
        assert!(!validate_identifier("_missing-prefix"));
        assert!(!validate_identifier(&"x".repeat(161)));
        assert!(!valid_uuid("pdcmd_not-a-uuid"));
        assert!(!valid_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(!valid_uuid("{5d79f33c-eb91-4af6-849e-a796b9b7501f}"));
        assert!(valid_uuid("5d79f33c-eb91-4af6-849e-a796b9b7501f"));

        let command = fixture_command(
            "pdcmd_envelope",
            "5d79f33c-eb91-4af6-849e-a796b9b7501f",
            "pause",
            json!({"schemaVersion":1,"assignmentId":"mpa_example","policyId":"mbp_example","revision":1,"stateGeneration":1}),
        );
        assert!(valid_command_envelope(
            &command,
            Some("5d79f33c-eb91-4af6-849e-a796b9b7501f")
        ));
        let mut invalid_command_id = command.clone();
        invalid_command_id.id = "_missing-prefix".into();
        assert!(!valid_command_envelope(&invalid_command_id, None));
        assert!(!valid_command_envelope(
            &command,
            Some("e626ae84-704a-42b8-b09d-61f2c69ba3ac")
        ));
        let mut excessive_attempts = command;
        excessive_attempts.max_attempts = 11;
        assert!(!valid_command_envelope(&excessive_attempts, None));
    }

    #[test]
    fn restore_contract_accepts_only_snapshot_id_and_generates_local_only_destination() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let command = fixture_command(
            "pdcmd_restore_destination_free",
            "394aee0e-c6dd-484c-8af1-35a3c7f66317",
            "restore_snapshot",
            json!({"schemaVersion":1,"snapshotId":"snapshot_exact"}),
        );
        let (ack, _) = process_command(&conn, &context, &organization, &command).unwrap();
        assert_eq!(ack.status, "succeeded");
        let tx = conn.unchecked_transaction().unwrap();
        let job = load_job(&tx, ack.result.as_ref().unwrap()["jobId"].as_str().unwrap()).unwrap();
        tx.commit().unwrap();
        let destination = PathBuf::from(job.destination_path.unwrap());
        assert_eq!(destination.parent(), Some(managed_restore_root().as_path()));
        assert!(safe_generated_restore_leaf(
            destination.file_name().unwrap().to_str().unwrap()
        ));
        assert_ne!(destination, managed_restore_destination_candidate());
        assert!(!ack
            .result
            .as_ref()
            .unwrap()
            .to_string()
            .contains("destination"));
        let serialized_events: String = conn
            .query_row(
                "SELECT group_concat(payload) FROM managed_control_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!serialized_events.contains("\"destinationPath\""));
        assert!(!serialized_events.contains("SaveState Restores"));
        for extra in [
            json!({"schemaVersion":1,"snapshotId":"snapshot_exact","destinationPath":r"C:\Users\Public\Startup"}),
            json!({"schemaVersion":1,"snapshotId":"snapshot_exact","overwrite":false}),
        ] {
            let invalid = fixture_command(
                &format!("pdcmd_restore_rejected_{}", uuid::Uuid::new_v4().simple()),
                "b9f194d2-f471-4418-9485-e0297b5e9c12",
                "restore_snapshot",
                extra,
            );
            let (ack, _) = process_command(&conn, &context, &organization, &invalid).unwrap();
            assert_eq!(result_code(&ack), Some("invalid_restore_snapshot"));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn generated_restore_root_is_protected_and_rejects_legacy_destination_paths() {
        let data = tempfile::tempdir().unwrap();
        let root = ensure_managed_restore_root_at(data.path()).unwrap();
        assert_eq!(root.path.file_name().unwrap(), "SaveState Restores");
        assert!(managed_restore_staging_acl_is_protected(&root.path).unwrap());
        assert!(std::fs::rename(&root.path, data.path().join("swapped-root")).is_err());
        let candidate = managed_restore_destination_candidate();
        let safe = validate_generated_managed_restore_destination(
            &candidate.to_string_lossy(),
            &root.path,
        )
        .unwrap();
        assert_eq!(safe.parent(), Some(root.path.as_path()));
        for unsafe_path in [
            r"C:\Users\Public\Startup\restore-394aee0e-c6dd-484c-8af1-35a3c7f66317",
            r"\\server\share\restore-394aee0e-c6dd-484c-8af1-35a3c7f66317",
            r"C:\Temp\restore-not-a-uuid",
        ] {
            assert!(
                validate_generated_managed_restore_destination(unsafe_path, &root.path).is_err()
            );
        }
        drop(root);
        let root_again = ensure_managed_restore_root_at(data.path()).unwrap();
        assert!(managed_restore_staging_acl_is_protected(&root_again.path).unwrap());
    }

    #[test]
    fn strict_policy_sync_is_isolated_and_read_only_projection_is_sanitized() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let command = fixture_command(
            "pdcmd_policy_sync",
            "6ea4f724-a0d7-4d14-8482-080f4bde30c2",
            "policy_sync",
            policy_payload("active", 1),
        );
        let (ack, _) = process_command(&conn, &context, &fixture_organization(), &command).unwrap();
        assert_eq!(ack.status, "succeeded");
        assert_eq!(ack.retryable, None);
        let policies = list_policies(
            &conn,
            &context.installation_id,
            &context.account.account_scope,
        )
        .unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].organization_name, "Example IT");
        assert!(policies[0].managed);
        assert_eq!(policies[0].desired_state, "active");
        assert!(!serde_json::to_string(&policies[0])
            .unwrap()
            .contains("secret-never-serialized"));

        let mut invalid = policy_payload("active", 2);
        invalid["definition"]["shell"] = json!("cmd.exe /c whoami");
        let invalid_command = fixture_command(
            "pdcmd_invalid_policy",
            "19550c9c-5a12-43c4-8123-92b46456a5ee",
            "policy_sync",
            invalid,
        );
        let (ack, _) =
            process_command(&conn, &context, &fixture_organization(), &invalid_command).unwrap();
        assert_eq!(ack.status, "failed");
        assert_eq!(result_code(&ack), Some("invalid_policy_sync"));
    }

    #[test]
    fn paused_policy_rejects_manual_run_and_resume_enables_one_replay_safe_job() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let sync = fixture_command(
            "pdcmd_policy_paused",
            "7edfe4b5-fd75-497f-b5cf-0ded3846beb5",
            "policy_sync",
            policy_payload("paused", 1),
        );
        process_command(&conn, &context, &organization, &sync).unwrap();

        let paused_run = fixture_command(
            "pdcmd_run_paused",
            "274aeadf-caf5-467c-968d-3d9c2dde3294",
            "run_backup",
            json!({
                "schemaVersion": 1,
                "assignmentId": "mpa_example",
                "policyId": "mbp_example",
                "revision": 1,
                "stateGeneration": 1
            }),
        );
        let (ack, _) = process_command(&conn, &context, &organization, &paused_run).unwrap();
        assert_eq!(result_code(&ack), Some("managed_policy_not_active"));

        let resume = fixture_command(
            "pdcmd_resume",
            "5427f849-0b4f-4b34-9440-a40d43a55589",
            "resume",
            json!({
                "schemaVersion": 1,
                "assignmentId": "mpa_example",
                "policyId": "mbp_example",
                "revision": 1,
                "stateGeneration": 2
            }),
        );
        let (resume_ack, _) = process_command(&conn, &context, &organization, &resume).unwrap();
        assert_eq!(
            resume_ack.result.as_ref().unwrap()["appliedStateGeneration"],
            2
        );
        let run = fixture_command(
            "pdcmd_run_once",
            "8e8f2bce-bdb1-41c0-a67e-a1160d57d02e",
            "run_backup",
            json!({
                "schemaVersion": 1,
                "assignmentId": "mpa_example",
                "policyId": "mbp_example",
                "revision": 1,
                "stateGeneration": 2
            }),
        );
        let (first, _) = process_command(&conn, &context, &organization, &run).unwrap();
        let first_job = first.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        let mut replay = run.clone();
        replay.lease_id = "6263ef0c-a2bd-4199-a78c-95763df45844".into();
        let (second, _) = process_command(&conn, &context, &organization, &replay).unwrap();
        assert_eq!(second.result, first.result);
        assert_eq!(second.retryable, None);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_jobs WHERE command_id = ?1 AND job_id = ?2",
                params![run.id, first_job],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let pause = fixture_command(
            "pdcmd_pause_after_queue",
            "314f1ec4-2861-40e0-89a0-1aec837c69fa",
            "pause",
            json!({
                "schemaVersion": 1,
                "assignmentId": "mpa_example",
                "policyId": "mbp_example",
                "revision": 1,
                "stateGeneration": 3
            }),
        );
        process_command(&conn, &context, &organization, &pause).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let queued_job = load_job(&tx, &first_job).unwrap();
        let paused_policy = load_policy(
            &tx,
            &context.installation_id,
            &context.account.account_scope,
            "mpa_example",
        )
        .unwrap()
        .unwrap();
        assert!(!managed_backup_job_matches_policy(
            &queued_job,
            &paused_policy
        ));
    }

    #[test]
    fn state_generation_rejects_out_of_order_pause_resume_and_stale_runs() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let sync = fixture_command(
            "pdcmd_generation_sync",
            "0218e239-794f-486b-8713-7d88e9a36191",
            "policy_sync",
            policy_payload("active", 1),
        );
        let (sync_ack, _) = process_command(&conn, &context, &organization, &sync).unwrap();
        assert_eq!(sync_ack.result.as_ref().unwrap()["appliedRevision"], 1);
        assert_eq!(
            sync_ack.result.as_ref().unwrap()["appliedStateGeneration"],
            1
        );

        let policy_action = |state_generation| {
            json!({
                "schemaVersion": 1,
                "assignmentId": "mpa_example",
                "policyId": "mbp_example",
                "revision": 1,
                "stateGeneration": state_generation
            })
        };
        let pause = fixture_command(
            "pdcmd_generation_pause",
            "de45dce7-12f4-4214-9040-adb1eea156ac",
            "pause",
            policy_action(2),
        );
        let resume = fixture_command(
            "pdcmd_generation_resume",
            "7e891bac-b6f9-48a2-8195-765b6d83c33f",
            "resume",
            policy_action(3),
        );
        assert_eq!(
            process_command(&conn, &context, &organization, &pause)
                .unwrap()
                .0
                .status,
            "succeeded"
        );
        assert_eq!(
            process_command(&conn, &context, &organization, &resume)
                .unwrap()
                .0
                .status,
            "succeeded"
        );
        let same_generation_resume = fixture_command(
            "pdcmd_generation_resume_idempotent",
            "a280a45c-4e66-4bca-b0b3-08645881ee36",
            "resume",
            policy_action(3),
        );
        let (same_resume_ack, _) =
            process_command(&conn, &context, &organization, &same_generation_resume).unwrap();
        assert_eq!(same_resume_ack.status, "succeeded");
        assert_eq!(
            same_resume_ack.result.as_ref().unwrap()["appliedStateGeneration"],
            3
        );

        let stale_pause = fixture_command(
            "pdcmd_generation_stale_pause",
            "00c0a03c-5962-441e-907a-9692563497ad",
            "pause",
            policy_action(2),
        );
        let (stale_pause_ack, _) =
            process_command(&conn, &context, &organization, &stale_pause).unwrap();
        assert_eq!(
            result_code(&stale_pause_ack),
            Some("policy_state_generation_conflict")
        );
        let same_generation_conflict = fixture_command(
            "pdcmd_generation_same_conflict",
            "0db0fe7c-aa73-40eb-a2a2-e06de733b252",
            "pause",
            policy_action(3),
        );
        let (same_generation_ack, _) =
            process_command(&conn, &context, &organization, &same_generation_conflict).unwrap();
        assert_eq!(
            result_code(&same_generation_ack),
            Some("policy_state_generation_conflict")
        );

        let stale_run = fixture_command(
            "pdcmd_generation_stale_run",
            "6ef423d5-b7de-4be4-89ea-6163c6139f30",
            "run_backup",
            policy_action(1),
        );
        let (stale_run_ack, _) =
            process_command(&conn, &context, &organization, &stale_run).unwrap();
        assert_eq!(
            result_code(&stale_run_ack),
            Some("policy_state_generation_conflict")
        );
        let current_run = fixture_command(
            "pdcmd_generation_current_run",
            "126d78bc-a777-4899-a080-13793c94eeb0",
            "run_backup",
            policy_action(3),
        );
        let (current_run_ack, _) =
            process_command(&conn, &context, &organization, &current_run).unwrap();
        assert_eq!(current_run_ack.status, "succeeded");
        assert_eq!(
            current_run_ack.result.as_ref().unwrap()["appliedStateGeneration"],
            3
        );
        let job_id = current_run_ack.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap();
        let stored_generation: i64 = conn
            .query_row(
                "SELECT policy_state_generation FROM managed_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_generation, 3);

        let policy = load_policy(
            &conn,
            &context.installation_id,
            &context.account.account_scope,
            "mpa_example",
        )
        .unwrap()
        .unwrap();
        assert_eq!(policy.desired_state, "active");
        assert_eq!(policy.state_generation, 3);
    }

    #[test]
    fn retryable_failure_reexecutes_only_after_a_new_lease() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let lease_one = "95d27ef4-d36b-45ec-8498-7b2717ae35ce";
        let command = fixture_command(
            "pdcmd_retry_policy",
            lease_one,
            "policy_sync",
            policy_payload("active", 1),
        );
        let hash = command_hash(&command).unwrap();
        conn.execute(
            "INSERT INTO managed_command_receipts
             (command_id, installation_id, command_hash, kind, lease_id, ack_status,
              ack_result, retryable, completed_at, ack_sent)
             VALUES (?1, ?2, ?3, ?4, ?5, 'failed', ?6, 1, ?7, 0)",
            params![
                command.id,
                context.installation_id,
                hash,
                command.kind,
                lease_one,
                json!({"code":"local_storage_unavailable"}).to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();

        let (same_lease, _) = process_command(&conn, &context, &organization, &command).unwrap();
        assert_eq!(same_lease.status, "failed");
        assert_eq!(same_lease.retryable, Some(true));
        assert!(list_policies(
            &conn,
            &context.installation_id,
            &context.account.account_scope
        )
        .unwrap()
        .is_empty());

        let mut new_lease = command.clone();
        new_lease.lease_id = "922ad56b-2480-4ffd-9604-62be74f63da5".into();
        new_lease.attempt = 2;
        let (retried, _) = process_command(&conn, &context, &organization, &new_lease).unwrap();
        assert_eq!(retried.status, "succeeded");
        assert_eq!(retried.retryable, None);
        assert_eq!(
            list_policies(
                &conn,
                &context.installation_id,
                &context.account.account_scope
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn removal_cancels_only_managed_scheduling_and_never_queues_snapshot_deletion() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let sync = fixture_command(
            "pdcmd_policy_active",
            "71d8b65d-fbb5-46f0-ac8b-c1a7ce8b5233",
            "policy_sync",
            policy_payload("active", 1),
        );
        process_command(&conn, &context, &organization, &sync).unwrap();
        let run = fixture_command(
            "pdcmd_run_before_remove",
            "c6b30e30-c96f-486a-8f4c-ae754379b2a4",
            "run_backup",
            json!({"schemaVersion":1,"assignmentId":"mpa_example","policyId":"mbp_example","revision":1,"stateGeneration":1}),
        );
        process_command(&conn, &context, &organization, &run).unwrap();
        let remove = fixture_command(
            "pdcmd_remove",
            "ed28f44a-6e98-4095-8477-63d20f3e09c1",
            "policy_sync",
            json!({
                "schemaVersion":1,"operation":"remove","assignmentId":"mpa_example",
                "policyId":"mbp_example","revision":2,"stateGeneration":2,
                "desiredState":"removed"
            }),
        );
        process_command(&conn, &context, &organization, &remove).unwrap();
        let (state, cancelled, deletes): (String, i64, i64) = conn
            .query_row(
                "SELECT p.desired_state,
                        (SELECT COUNT(*) FROM managed_jobs WHERE assignment_id = p.assignment_id AND cancel_requested = 1),
                        (SELECT COUNT(*) FROM managed_jobs WHERE kind = 'delete_snapshot')
                   FROM managed_backup_policies p WHERE assignment_id = 'mpa_example'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "removed");
        assert_eq!(cancelled, 1);
        assert_eq!(deletes, 0);
    }

    #[test]
    fn snapshot_delete_command_is_replay_safe_and_reports_delete_only_after_success() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let command = fixture_command(
            "pdcmd_delete_snapshot",
            "178602a0-5c1d-44cc-bbea-5fc2f632a11c",
            "delete_snapshot",
            json!({"schemaVersion":1,"snapshotId":"snapshot_exact"}),
        );
        let (first, _) = process_command(&conn, &context, &organization, &command).unwrap();
        let mut replay = command.clone();
        replay.lease_id = "45f18a90-f3a3-4f95-a860-1f424821a801".into();
        let (second, _) = process_command(&conn, &context, &organization, &replay).unwrap();
        assert_eq!(first.result, second.result);
        let (jobs, deleted_events): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM managed_jobs WHERE command_id = ?1),
                        (SELECT COUNT(*) FROM managed_control_events WHERE payload LIKE '%snapshot_deleted%')",
                params![command.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(jobs, 1);
        assert_eq!(deleted_events, 0);
        let payload: String = conn
            .query_row(
                "SELECT payload FROM managed_control_events LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["type"], "job_queued");
        assert_eq!(payload["jobKind"], "snapshot_delete");
    }

    #[test]
    fn snapshot_matching_and_events_use_exact_ids_and_sanitized_metrics() {
        let snapshots = vec![crate::kopia::KopiaSnapshot {
            id: "snapshot-one".into(),
            source_path: r"C:\Secret Customer".into(),
            start_time: "2026-09-14T10:00:00Z".into(),
            size: 4096,
            file_count: 12,
            folder: "/Managed".into(),
            backup_kind: "files".into(),
            database_profile_id: None,
            database_profile_name: None,
            root_object_id: None,
            profile_id: Some("mpa_example".into()),
            profile_name: Some("Secret policy name".into()),
            trigger: Some("managed_manual".into()),
            version_number: Some(1),
        }];
        assert!(exact_snapshot(&snapshots, "snapshot").is_none());
        assert!(exact_snapshot(&snapshots, "snapshot-one-extra").is_none());
        assert_eq!(
            exact_snapshot(&snapshots, "snapshot-one").unwrap().size,
            4096
        );

        let conn = fixture_db();
        let tx = conn.unchecked_transaction().unwrap();
        let job = ManagedJob {
            job_id: "job_one".into(),
            installation_id: "installation-one".into(),
            owner_account: "owner@example.invalid::service:9".into(),
            command_id: Some("pdcmd_backup".into()),
            kind: "backup".into(),
            trigger_kind: "managed_manual".into(),
            assignment_id: Some("mpa_example".into()),
            policy_id: Some("mbp_example".into()),
            policy_revision: Some(1),
            policy_state_generation: Some(1),
            snapshot_id: None,
            destination_path: None,
            restore_staging_path: None,
            cancel_requested: false,
        };
        queue_snapshot_event_tx(
            &tx,
            &job,
            "snapshot_available",
            "snapshot-one",
            Some(&snapshots[0]),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        let payload: String = conn
            .query_row("SELECT payload FROM managed_control_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["jobId"], "job_one");
        assert_eq!(payload["jobKind"], "backup");
        assert_eq!(payload["logicalBytes"], 4096);
        assert_eq!(payload["fileCount"], 12);
        assert!(payload.get("storedBytes").is_none());
        let encoded = payload.to_string();
        assert!(!encoded.contains("Secret Customer"));
        assert!(!encoded.contains("Secret policy name"));

        let tx = conn.unchecked_transaction().unwrap();
        queue_snapshot_event_tx(
            &tx,
            &job,
            "snapshot_deleted",
            "snapshot-retained-out",
            None,
            Some("retention"),
        )
        .unwrap();
        let administrator_job = ManagedJob {
            job_id: "job_delete".into(),
            installation_id: "installation-one".into(),
            owner_account: "owner@example.invalid::service:9".into(),
            command_id: Some("pdcmd_delete".into()),
            kind: "delete_snapshot".into(),
            trigger_kind: "managed_delete".into(),
            assignment_id: None,
            policy_id: None,
            policy_revision: None,
            policy_state_generation: None,
            snapshot_id: Some("snapshot-admin".into()),
            destination_path: None,
            restore_staging_path: None,
            cancel_requested: false,
        };
        queue_snapshot_event_tx(
            &tx,
            &administrator_job,
            "snapshot_deleted",
            "snapshot-admin",
            None,
            Some("administrator"),
        )
        .unwrap();
        tx.commit().unwrap();
        let mut statement = conn
            .prepare(
                "SELECT payload FROM managed_control_events
                  WHERE payload LIKE '%deletionReason%' ORDER BY created_at, event_id",
            )
            .unwrap();
        let deletion_events = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str::<Value>(&row.unwrap()).unwrap())
            .collect::<Vec<_>>();
        let retention = deletion_events
            .iter()
            .find(|event| event["deletionReason"] == "retention")
            .unwrap();
        assert_eq!(retention["jobId"], "job_one");
        assert_eq!(retention["jobKind"], "backup");
        assert_eq!(retention["assignmentId"], "mpa_example");
        assert!(retention.get("commandId").is_none());
        let administrator = deletion_events
            .iter()
            .find(|event| event["deletionReason"] == "administrator")
            .unwrap();
        assert_eq!(administrator["jobId"], "job_delete");
        assert_eq!(administrator["jobKind"], "snapshot_delete");
        assert_eq!(administrator["commandId"], "pdcmd_delete");
    }

    #[test]
    fn queued_jobs_are_scoped_to_the_captured_account_context() {
        let conn = fixture_db();
        let owner = fixture_context("owner@example.invalid::service:9");
        let other = fixture_context("other@example.invalid::service:9");
        let tx = conn.unchecked_transaction().unwrap();
        insert_job(
            &tx,
            &owner,
            Some("pdcmd_scoped"),
            "delete_snapshot",
            "managed_delete",
            None,
            None,
            None,
            None,
            Some("snapshot-one"),
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(
            claim_next_job(&conn, &other.installation_id, &other.account.account_scope)
                .unwrap()
                .is_none()
        );
        assert!(
            claim_next_job(&conn, &owner.installation_id, &owner.account.account_scope)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn startup_reconciliation_fails_running_jobs_once_and_retains_queued_work() {
        let conn = fixture_db();
        let context = fixture_context("owner@example.invalid::service:9");
        let tx = conn.unchecked_transaction().unwrap();
        let running = insert_job(
            &tx,
            &context,
            Some("pdcmd_running"),
            "delete_snapshot",
            "managed_delete",
            None,
            None,
            None,
            None,
            Some("snapshot-one"),
            None,
        )
        .unwrap();
        let queued = insert_job(
            &tx,
            &context,
            Some("pdcmd_queued"),
            "delete_snapshot",
            "managed_delete",
            None,
            None,
            None,
            None,
            Some("snapshot-two"),
            None,
        )
        .unwrap();
        tx.execute(
            "UPDATE managed_jobs SET status = 'running', started_at = ?1 WHERE job_id = ?2",
            params![Utc::now().to_rfc3339(), running],
        )
        .unwrap();
        tx.commit().unwrap();
        reconcile_startup(&conn).unwrap();
        let running_status: String = conn
            .query_row(
                "SELECT status FROM managed_jobs WHERE job_id = ?1",
                params![running],
                |row| row.get(0),
            )
            .unwrap();
        let queued_status: String = conn
            .query_row(
                "SELECT status FROM managed_jobs WHERE job_id = ?1",
                params![queued],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(running_status, "interrupted");
        assert_eq!(queued_status, "queued");
        reconcile_startup(&conn).unwrap();
        let restart_failures: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_control_events
                  WHERE payload LIKE '%device_restarted%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restart_failures, 1);
    }

    #[test]
    fn local_history_cleanup_is_bounded_without_dropping_pending_delivery() {
        let mut conn = fixture_db();
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction().unwrap();
        for index in 0..2001 {
            tx.execute(
                "INSERT INTO managed_command_receipts
                 (command_id, installation_id, command_hash, kind, lease_id, ack_status,
                  ack_result, retryable, completed_at, ack_sent)
                 VALUES (?1, 'installation-one', 'hash', 'pause', ?2, 'succeeded', '{}', NULL, ?3, 1)",
                params![format!("receipt-{index}"), uuid::Uuid::new_v4().to_string(), now],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO managed_command_receipts
             (command_id, installation_id, command_hash, kind, lease_id, ack_status,
              ack_result, retryable, completed_at, ack_sent)
             VALUES ('pending-receipt', 'installation-one', 'hash', 'pause', ?1,
                     'succeeded', '{}', NULL, ?2, 0)",
            params![uuid::Uuid::new_v4().to_string(), now],
        )
        .unwrap();
        for index in 0..5001 {
            tx.execute(
                "INSERT INTO managed_control_events
                 (event_id, installation_id, payload, created_at, sent_at)
                 VALUES (?1, 'installation-one', '{}', ?2, ?2)",
                params![format!("event-{index}"), now],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO managed_control_events
             (event_id, installation_id, payload, created_at, sent_at)
             VALUES ('pending-event', 'installation-one', '{}', ?1, NULL)",
            params![now],
        )
        .unwrap();
        for index in 0..2001 {
            tx.execute(
                "INSERT INTO managed_jobs
                 (job_id, installation_id, owner_account, kind, trigger_kind, status,
                  created_at, completed_at)
                 VALUES (?1, 'installation-one', 'owner', 'backup', 'managed_manual',
                         'succeeded', ?2, ?2)",
                params![format!("job-{index}"), now],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO managed_jobs
             (job_id, installation_id, owner_account, kind, trigger_kind, status, created_at)
             VALUES ('pending-job', 'installation-one', 'owner', 'backup',
                     'managed_manual', 'queued', ?1)",
            params![now],
        )
        .unwrap();
        tx.commit().unwrap();

        cleanup_local_history(&conn).unwrap();
        let sent_receipts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_command_receipts WHERE ack_sent = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sent_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_control_events WHERE sent_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let terminal_jobs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM managed_jobs WHERE status = 'succeeded'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (sent_receipts, sent_events, terminal_jobs),
            (2000, 5000, 2000)
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM managed_command_receipts WHERE command_id = 'pending-receipt'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM managed_control_events WHERE event_id = 'pending-event'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM managed_jobs WHERE job_id = 'pending-job'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn idle_poll_interval_and_error_backoff_are_bounded() {
        assert_eq!(control_poll_delay_seconds(0), 30);
        assert_eq!(control_poll_delay_seconds(1), 60);
        assert_eq!(control_poll_delay_seconds(2), 120);
        assert_eq!(control_poll_delay_seconds(3), 240);
        assert_eq!(control_poll_delay_seconds(4), 300);
        assert_eq!(control_poll_delay_seconds(100), 300);
    }

    #[tokio::test]
    async fn pending_ack_unauthorized_is_detected_before_poll_and_local_work_is_stopped() {
        let conn = fixture_db();
        let mut context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let sync = fixture_command(
            "pdcmd_revoked_pending_ack",
            "988918b4-516c-4201-9f36-d100df981215",
            "policy_sync",
            policy_payload("active", 1),
        );
        process_command(&conn, &context, &organization, &sync).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let queued_job = insert_job(
            &tx,
            &context,
            Some("pdcmd_revoked_queued_job"),
            "backup",
            "managed_manual",
            Some("mpa_example"),
            Some("mbp_example"),
            Some(1),
            Some(1),
            None,
            None,
        )
        .unwrap();
        tx.commit().unwrap();

        let (base_url, server) = one_shot_http_error(
            "401 Unauthorized",
            r#"{"error":"invalid_device_credential"}"#,
        );
        context.account.api.base_url = base_url;
        let state = AppStateWrapper(std::sync::Mutex::new(crate::state::AppState::new(
            SaveStateClient::new("desktop-fixture".into()),
            conn,
        )));
        let error = flush_pending_acks(&context, &state).await.unwrap_err();
        server.join().unwrap();
        assert!(crate::organization_enrollment::device_credential_was_revoked(&error));

        // This is the same local cleanup invoked by the revoked-delivery branch
        // before the next poll. The secure credential-store removal is covered
        // by organization_enrollment's injected-store tests.
        disconnect_local_managed_state(
            &state,
            &context.installation_id,
            &context.account.account_scope,
        )
        .await
        .unwrap();
        let guard = state.0.lock().unwrap();
        let desired_state: String = guard
            .db
            .query_row(
                "SELECT desired_state FROM managed_backup_policies WHERE assignment_id = 'mpa_example'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let job_status: String = guard
            .db
            .query_row(
                "SELECT status FROM managed_jobs WHERE job_id = ?1",
                params![queued_job],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desired_state, "removed");
        assert_eq!(job_status, "cancelled");
    }

    #[tokio::test]
    async fn disconnect_reuses_request_and_only_tombstones_managed_local_state() {
        let conn = fixture_db();
        conn.execute_batch(
            "CREATE TABLE personal_profile_sentinel (id TEXT PRIMARY KEY);
             INSERT INTO personal_profile_sentinel (id) VALUES ('personal-profile');
             CREATE TABLE snapshot_sentinel (id TEXT PRIMARY KEY);
             INSERT INTO snapshot_sentinel (id) VALUES ('snapshot-preserved');",
        )
        .unwrap();
        let context = fixture_context("owner@example.invalid::service:9");
        let organization = fixture_organization();
        let sync = fixture_command(
            "pdcmd_disconnect_policy",
            "0dc89c9e-4779-450e-a542-16aa428159e8",
            "policy_sync",
            policy_payload("active", 1),
        );
        process_command(&conn, &context, &organization, &sync).unwrap();
        let request_one = disconnect_request_id(
            &conn,
            &context.installation_id,
            &context.account.account_scope,
        )
        .unwrap();
        let request_two = disconnect_request_id(
            &conn,
            &context.installation_id,
            &context.account.account_scope,
        )
        .unwrap();
        assert_eq!(request_one, request_two);
        assert!(valid_uuid(&request_one));

        let tx = conn.unchecked_transaction().unwrap();
        let queued_job = insert_job(
            &tx,
            &context,
            Some("pdcmd_disconnect_queued"),
            "backup",
            "managed_manual",
            Some("mpa_example"),
            Some("mbp_example"),
            Some(1),
            Some(1),
            None,
            None,
        )
        .unwrap();
        let running_job = insert_job(
            &tx,
            &context,
            Some("pdcmd_disconnect_running"),
            "delete_snapshot",
            "managed_delete",
            None,
            None,
            None,
            None,
            Some("snapshot-preserved"),
            None,
        )
        .unwrap();
        tx.execute(
            "UPDATE managed_jobs SET status = 'running', started_at = ?1 WHERE job_id = ?2",
            params![Utc::now().to_rfc3339(), running_job],
        )
        .unwrap();
        tx.commit().unwrap();
        let state = AppStateWrapper(std::sync::Mutex::new(crate::state::AppState::new(
            SaveStateClient::new("desktop-fixture".into()),
            conn,
        )));
        assert_eq!(
            disconnect_local_managed_state(
                &state,
                &context.installation_id,
                &context.account.account_scope
            )
            .await
            .unwrap(),
            1
        );
        let guard = state.0.lock().unwrap();
        let desired_state: String = guard
            .db
            .query_row(
                "SELECT desired_state FROM managed_backup_policies WHERE assignment_id = 'mpa_example'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let queued_status: String = guard
            .db
            .query_row(
                "SELECT status FROM managed_jobs WHERE job_id = ?1",
                params![queued_job],
                |row| row.get(0),
            )
            .unwrap();
        let running_cancelled: i64 = guard
            .db
            .query_row(
                "SELECT cancel_requested FROM managed_jobs WHERE job_id = ?1",
                params![running_job],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desired_state, "removed");
        assert_eq!(queued_status, "cancelled");
        assert_eq!(running_cancelled, 1);
        for table in ["personal_profile_sentinel", "snapshot_sentinel"] {
            let count: i64 = guard
                .db
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "disconnect changed {table}");
        }
        let pending_disconnect: i64 = guard
            .db
            .query_row("SELECT COUNT(*) FROM managed_disconnect_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending_disconnect, 0);
    }
}
