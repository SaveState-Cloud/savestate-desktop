use crate::api::EngineScheduleSnapshot;
use crate::backup;
use crate::db::{self, BackupProfile};
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, LocalResult, NaiveDate, TimeZone, Timelike, Utc};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ScheduleReportState {
    last_success: Option<(String, Instant)>,
    in_flight: Option<String>,
}

static SCHEDULE_REPORT_STATE: OnceLock<Mutex<ScheduleReportState>> = OnceLock::new();
const LOCAL_SCHEDULE_MIGRATION_KEY: &str = "schedule_times_machine_local_v1";

/// Existing JSON schedules were previously interpreted as UTC even though the
/// UI described ordinary wall-clock times. Recompute them once on upgrade so
/// an existing 14:15 profile does not remain stuck at 14:15 UTC until edited.
pub fn migrate_schedule_times_to_local(conn: &rusqlite::Connection) -> Result<()> {
    migrate_schedule_times_in_timezone(conn, Utc::now(), &Local)
}

fn migrate_schedule_times_in_timezone<Tz: TimeZone>(
    conn: &rusqlite::Connection,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    if db::get_app_metadata(&transaction, LOCAL_SCHEDULE_MIGRATION_KEY)?.as_deref() == Some("done")
    {
        return Ok(());
    }

    for mut profile in db::list_profiles(&transaction)? {
        let Some(schedule) = profile.schedule.as_deref() else {
            continue;
        };
        if !profile.enabled || schedule.trim().is_empty() {
            continue;
        }

        let next_run = if schedule.trim_start().starts_with('{') {
            compute_migrated_json_next_run(
                schedule,
                profile.next_run.as_deref(),
                profile
                    .last_run
                    .as_deref()
                    .or(Some(profile.created_at.as_str())),
                now,
                timezone,
            )
        } else {
            // Legacy presets and cron expressions now follow the same machine-
            // local clock contract as schedules created by the current UI.
            compute_next_run(profile.schedule.as_deref())
        };
        let Some(next_run) = next_run else { continue };
        profile.next_run = Some(next_run);
        db::update_profile(&transaction, &profile)?;
    }
    db::set_app_metadata(&transaction, LOCAL_SCHEDULE_MIGRATION_KEY, "done")?;
    transaction.commit()?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands for backup profile management
// ────────────────────────────────────────────────────────────────────

/// Create a new backup profile. Returns the created profile.
#[tauri::command]
pub async fn cmd_create_profile(
    state: tauri::State<'_, AppStateWrapper>,
    name: String,
    source_path: String,
    schedule: Option<String>,
    retention: i64,
    folder: Option<String>,
) -> std::result::Result<BackupProfile, String> {
    let owner_account = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard
            .account_scope()
            .ok_or_else(|| "Sign in before creating a backup profile".to_string())?
    };
    let profile = BackupProfile {
        id: uuid::Uuid::new_v4().to_string(),
        owner_account: Some(owner_account),
        name,
        source_path,
        schedule: schedule.clone(),
        retention,
        folder: folder.unwrap_or_else(|| "/".to_string()),
        enabled: true,
        last_run: None,
        next_run: compute_next_run(schedule.as_deref()),
        retry_count: 0,
        retry_at: None,
        last_error: None,
        last_error_code: None,
        schedule_state: "scheduled".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        db::create_profile(&guard.db, &profile).map_err(|e| e.to_string())?;
    }

    report_schedule_snapshot(&state);

    Ok(profile)
}

/// Update an existing backup profile. Returns the updated profile.
#[tauri::command]
pub async fn cmd_update_profile(
    state: tauri::State<'_, AppStateWrapper>,
    id: String,
    name: String,
    source_path: String,
    schedule: Option<String>,
    retention: i64,
    enabled: bool,
    folder: Option<String>,
) -> std::result::Result<BackupProfile, String> {
    let profile = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before editing a backup profile".to_string())?;
        let mut existing = db::get_profile_for_account(&guard.db, &id, &owner_account)
            .map_err(|e| e.to_string())?;
        existing.name = name;
        existing.source_path = source_path;
        existing.schedule = schedule.clone();
        existing.retention = retention;
        existing.folder = folder.unwrap_or_else(|| "/".to_string());
        existing.enabled = enabled;
        existing.next_run = if enabled {
            compute_next_run(schedule.as_deref())
        } else {
            None
        };
        existing.retry_count = 0;
        existing.retry_at = None;
        existing.last_error = None;
        existing.last_error_code = None;
        existing.schedule_state = if enabled { "scheduled" } else { "disabled" }.to_string();
        db::update_profile(&guard.db, &existing).map_err(|e| e.to_string())?;
        existing
    };

    report_schedule_snapshot(&state);

    Ok(profile)
}

/// Delete a backup profile by ID.
#[tauri::command]
pub async fn cmd_delete_profile(
    state: tauri::State<'_, AppStateWrapper>,
    id: String,
) -> std::result::Result<(), String> {
    {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before deleting a backup profile".to_string())?;
        db::delete_profile_for_account(&guard.db, &id, &owner_account)
            .map_err(|e| e.to_string())?;
    }
    report_schedule_snapshot(&state);
    Ok(())
}

/// List all backup profiles.
#[tauri::command]
pub async fn cmd_list_profiles(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<BackupProfile>, String> {
    let profiles = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before listing backup profiles".to_string())?;
        db::list_profiles_for_account(&guard.db, &owner_account).map_err(|e| e.to_string())?
    };
    report_schedule_snapshot_from_profiles(&state, &profiles);
    Ok(profiles)
}

/// Count profiles created before account ownership was introduced. They stay
/// hidden and cannot be scheduled until the user explicitly claims them.
#[tauri::command]
pub async fn cmd_count_unowned_profiles(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<u64, String> {
    let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
    guard
        .account_scope()
        .ok_or_else(|| "Sign in before checking legacy profiles".to_string())?;
    db::count_unowned_profiles(&guard.db).map_err(|e| e.to_string())
}

/// Assign legacy profiles to the current account only after explicit user
/// confirmation in the UI.
#[tauri::command]
pub async fn cmd_claim_unowned_profiles(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<u64, String> {
    let claimed = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before claiming legacy profiles".to_string())?;
        db::claim_unowned_profiles(&guard.db, &owner_account).map_err(|e| e.to_string())?
    };
    report_schedule_snapshot(&state);
    Ok(claimed)
}

fn engine_schedule(profile: &BackupProfile) -> Option<EngineScheduleSnapshot> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ScheduleConfig {
        times: Vec<String>,
        interval_days: u32,
    }

    let schedule = profile.schedule.as_deref()?.trim();
    if schedule.is_empty() {
        return None;
    }
    let (times, interval_days) = if schedule.starts_with('{') {
        let config: ScheduleConfig = serde_json::from_str(schedule).ok()?;
        (config.times, config.interval_days)
    } else {
        match schedule {
            "hourly" => ((0..24).map(|hour| format!("{:02}:00", hour)).collect(), 1),
            "every_6h" => (
                vec![
                    "00:00".into(),
                    "06:00".into(),
                    "12:00".into(),
                    "18:00".into(),
                ],
                1,
            ),
            "daily" => (vec!["00:00".into()], 1),
            "weekly" => (vec!["00:00".into()], 7),
            // Preserve custom cron schedules in the list through next_run_at;
            // exact hourly load cannot be inferred safely from arbitrary cron.
            _ => (Vec::new(), 1),
        }
    };

    Some(EngineScheduleSnapshot {
        profile_id: profile.id.clone(),
        times,
        interval_days,
        next_run_at: profile.next_run.clone(),
        retry_at: profile.retry_at.clone(),
        retry_count: profile.retry_count,
        state: profile.schedule_state.clone(),
        last_error_code: profile.last_error_code.clone(),
        enabled: profile.enabled,
    })
}

pub fn report_schedule_snapshot(state: &AppStateWrapper) {
    let profiles = {
        let Ok(guard) = state.0.lock() else { return };
        let Some(owner_account) = guard.account_scope() else {
            return;
        };
        let Ok(profiles) = db::list_profiles_for_account(&guard.db, &owner_account) else {
            return;
        };
        profiles
    };
    report_schedule_snapshot_from_profiles(state, &profiles);
}

pub fn report_schedule_snapshot_from_profiles(state: &AppStateWrapper, profiles: &[BackupProfile]) {
    let (api, owner_account) = {
        let Ok(guard) = state.0.lock() else { return };
        let Some(owner_account) = guard.account_scope() else {
            return;
        };
        (guard.api.clone(), owner_account)
    };
    let schedules: Vec<EngineScheduleSnapshot> = profiles
        .iter()
        .filter(|profile| profile.owner_account.as_deref() == Some(owner_account.as_str()))
        .filter_map(engine_schedule)
        .collect();
    // Include the local account scope so switching between accounts with the
    // same schedule shape still reports to the newly authenticated account.
    let Ok(fingerprint) = serde_json::to_string(&(owner_account, &schedules)) else {
        return;
    };
    let now = Instant::now();
    let should_report = SCHEDULE_REPORT_STATE
        .get_or_init(|| Mutex::new(ScheduleReportState::default()))
        .lock()
        .map(|mut report| {
            let unchanged_and_fresh = report
                .last_success
                .as_ref()
                .map(|(previous, sent_at)| {
                    previous == &fingerprint && sent_at.elapsed() < Duration::from_secs(15 * 60)
                })
                .unwrap_or(false);
            let already_sending = report.in_flight.as_deref() == Some(fingerprint.as_str());
            if !unchanged_and_fresh && !already_sending {
                report.in_flight = Some(fingerprint.clone());
            }
            !unchanged_and_fresh && !already_sending
        })
        .unwrap_or(true);
    if !should_report {
        return;
    }

    tokio::spawn(async move {
        let delivered = api.send_schedule_snapshot(schedules).await.is_ok();
        if let Ok(mut report) = SCHEDULE_REPORT_STATE
            .get_or_init(|| Mutex::new(ScheduleReportState::default()))
            .lock()
        {
            if delivered {
                report.last_success = Some((fingerprint.clone(), now));
            }
            if report.in_flight.as_deref() == Some(fingerprint.as_str()) {
                report.in_flight = None;
            }
        }
    });
}

/// Run a backup for a specific profile. Returns the backup ID.
#[tauri::command]
pub async fn cmd_run_profile_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    profile_id: String,
) -> std::result::Result<String, String> {
    let (api, profile_name) = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {}", error))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before running a backup profile".to_string())?;
        let profile = db::get_profile_for_account(&guard.db, &profile_id, &owner_account)
            .map_err(|error| error.to_string())?;
        (guard.api.clone(), profile.name)
    };

    let result = run_profile_backup_inner(app, &state, &profile_id, "manual").await;
    match &result {
        Ok(backup_id) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_success",
                &profile_name,
                &format!("Manual profile backup completed. ID: {}", backup_id),
            )
            .await;
        }
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_failure",
                &profile_name,
                "Manual profile backup failed. Open SaveState Vault for details.",
            )
            .await;
        }
    }
    result.map_err(|error| error.to_string())
}

/// Inner implementation for running a profile backup.
/// This is also called by the scheduler.
pub async fn run_profile_backup_inner(
    app: tauri::AppHandle,
    state: &AppStateWrapper,
    profile_id: &str,
    trigger: &'static str,
) -> Result<String> {
    // Keep the full scheduled/profile workflow under one operation lease so
    // an update cannot slip into the gap between snapshot creation, retention,
    // and recording the next run time.
    let _operation_guard = crate::kopia::begin_operation().await;

    // 1. Load the profile
    let (profile, owner_account) = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| anyhow!("Sign in before running a backup profile"))?;
        let profile = db::get_profile_for_account(&guard.db, profile_id, &owner_account)?;
        (profile, owner_account)
    };

    // 2. Verify source path exists
    let source = std::path::PathBuf::from(&profile.source_path);
    if !source.exists() {
        return Err(anyhow!(
            "Source path does not exist: {}",
            profile.source_path
        ));
    }

    // 3. Run Kopia backup pipeline (dedup + compression + B2 upload)
    let backup_id = crate::kopia::backup_path_with_trigger(
        &app,
        state,
        &profile.source_path,
        trigger,
        &profile.folder,
    )
    .await?;

    if profile.retention > 0 {
        crate::kopia::set_retention(&app, state, profile.retention as u32).await?;
    }

    {
        let api = {
            let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
            guard.api.clone()
        };
        let _ = api.enforce_retention().await;
    }

    // 5. Update profile run times
    let now = chrono::Utc::now().to_rfc3339();
    let next = compute_next_run(profile.schedule.as_deref());
    {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        if guard.account_scope().as_deref() != Some(owner_account.as_str()) {
            return Err(anyhow!(
                "The signed-in account changed while the profile was running"
            ));
        }
        db::update_profile_run_times(&guard.db, profile_id, &owner_account, &now, next.as_deref())?;
    }

    Ok(backup_id)
}

pub fn get_dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    size += get_dir_size(&entry.path());
                } else {
                    size += meta.len();
                }
            }
        }
    }
    size
}

/// Execute the full backup pipeline for a profile (V2 Streaming).
async fn run_profile_backup_pipeline(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    source: &std::path::Path,
    profile: &BackupProfile,
    master_key: &[u8; 32],
) -> Result<String> {
    use rand::RngCore;
    use tauri::Emitter;

    let backup_id = uuid::Uuid::new_v4().to_string();

    let original_size = 0;

    let display_name = source
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let timestamp = chrono::Utc::now().format("%Y-%m-%d_%H%M%S").to_string();
    let upload_filename = format!("{}_{}.stream.enc", display_name, timestamp);

    let api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        guard.api.clone()
    };

    let _ = app.emit(
        "backup-progress",
        &backup::BackupProgress {
            id: backup_id.clone(),
            stage: "compressing".to_string(),
            progress: 0.1,
            message: format!("Initializing backup '{}'…", profile.name),
        },
    );

    let multipart_res = api
        .multipart_create(
            &upload_filename,
            original_size as u64,
            "application/octet-stream",
            None,
        )
        .await?;

    let upload_id = multipart_res.upload_id;
    let upload_key = multipart_res.key;

    {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        db::record_backup_start(
            &guard.db,
            &backup_id,
            &upload_filename,
            &upload_key,
            original_size,
        )?;
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(6);
    let source_clone = source.to_path_buf();
    let profile_name = profile.name.replace(' ', "_");

    let stream_task = tokio::task::spawn_blocking(move || -> Result<()> {
        let sender = backup::ChunkSender {
            tx,
            buffer: Vec::with_capacity(backup::CHUNK_SIZE_PLAINTEXT),
        };
        let mut encoder = zstd::Encoder::new(sender, 3)?;
        {
            let mut builder = tar::Builder::new(&mut encoder);
            if source_clone.is_dir() {
                for entry in walkdir::WalkDir::new(&source_clone)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let path = entry.path();
                    if path.is_file() {
                        let rel_path = path.strip_prefix(&source_clone).unwrap_or(path);
                        if let Ok(mut file) = std::fs::File::open(path) {
                            let name = std::path::Path::new(&profile_name).join(rel_path);
                            let name_str = name.to_string_lossy().replace('\\', "/");
                            let _ = builder.append_file(&name_str, &mut file);
                        }
                    }
                }
            } else {
                let mut file = std::fs::File::open(&source_clone)?;
                let file_name = source_clone
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                builder.append_file(&file_name, &mut file)?;
            }
            builder.finish()?;
        }
        let mut sender = encoder.finish()?;
        use std::io::Write;
        sender.flush()?;
        Ok(())
    });

    let mut base_nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut base_nonce);

    let mut part_number = 1;
    let mut total_encrypted_size = 0;

    let app_handle = app.clone();
    let backup_id_clone = backup_id.clone();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(6));
    let mut upload_tasks = Vec::new();

    while let Some(chunk) = rx.recv().await {
        let encrypted = backup::encrypt_chunk_v2(&chunk, &base_nonce, part_number, master_key)?;
        total_encrypted_size += encrypted.len() as i64;

        let _ = app_handle.emit(
            "backup-progress",
            &backup::BackupProgress {
                id: backup_id_clone.clone(),
                stage: "uploading".to_string(),
                progress: 0.5,
                message: format!("Uploading part {}...", part_number),
            },
        );

        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let api_clone = api.clone();
        let upload_key_clone = upload_key.clone();
        let upload_id_clone = upload_id.clone();
        let pn = part_number;

        let task = tokio::spawn(async move {
            let presign = api_clone
                .multipart_presign_part(&upload_key_clone, &upload_id_clone, pn)
                .await?;
            let etag = api_clone
                .upload_part_to_presigned_url(
                    &presign.upload_url,
                    encrypted,
                    backup::CHUNK_SIZE_PLAINTEXT as u64,
                )
                .await?;
            drop(permit);
            Ok::<crate::api::MultipartPart, anyhow::Error>(crate::api::MultipartPart {
                part_number: pn,
                etag,
            })
        });

        upload_tasks.push(task);
        part_number += 1;
    }

    if let Err(e) = stream_task
        .await
        .map_err(|e| anyhow!("Stream task panicked: {}", e))?
    {
        let guard = state.0.lock().map_err(|e2| anyhow!("Lock: {}", e2))?;
        let _ = db::record_backup_failed(&guard.db, &backup_id, &e.to_string());
        return Err(e);
    }

    let mut parts = Vec::new();
    for task in upload_tasks {
        let part = task.await.map_err(|e| anyhow!("Task panicked: {}", e))??;
        parts.push(part);
    }
    parts.sort_by_key(|p| p.part_number);

    api.multipart_complete(&upload_key, &upload_id, parts)
        .await?;

    {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        db::record_backup_complete(&guard.db, &backup_id, total_encrypted_size)?;
    }

    let _ = app.emit(
        "backup-progress",
        &backup::BackupProgress {
            id: backup_id.clone(),
            stage: "manifest".to_string(),
            progress: 0.9,
            message: "Saving manifest…".to_string(),
        },
    );

    let manifest = build_simple_manifest(&profile.name, source);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    let manifest_key = format!("{}_manifest.json", upload_key.trim_end_matches(".enc"));

    let manifest_result: Result<()> = async {
        let manifest_presign = api
            .presign_manifest(&manifest_key, "application/json")
            .await?;
        api.upload_to_presigned_url(
            &manifest_presign.upload_url,
            manifest_bytes.clone(),
            "application/json",
            manifest_bytes.len() as u64,
        )
        .await
    }
    .await;

    if let Err(e) = manifest_result {
        eprintln!("Warning: Failed to upload manifest: {}", e);
    }

    let _ = app.emit(
        "backup-progress",
        &backup::BackupProgress {
            id: backup_id.clone(),
            stage: "completed".to_string(),
            progress: 1.0,
            message: "Done!".to_string(),
        },
    );

    crate::notifications::send_backup_notification(
        &api,
        "backup_success",
        &profile.name,
        &format!("Manual backup completed. ID: {}", backup_id),
    )
    .await;

    Ok(backup_id)
}

/// Build a simple manifest JSON by walking a source path.
fn build_simple_manifest(profile_name: &str, source: &std::path::Path) -> serde_json::Value {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut files = Vec::new();

    if source.is_dir() {
        if let Ok(entries) = walkdir::WalkDir::new(source)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
        {
            for entry in entries {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(source)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default();
                files.push(serde_json::json!({
                    "path": rel,
                    "size": size,
                    "modified": modified,
                }));
            }
        }
    } else if source.is_file() {
        let size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
        let modified = std::fs::metadata(source)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.to_rfc3339()
            })
            .unwrap_or_default();
        let name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        files.push(serde_json::json!({
            "path": name,
            "size": size,
            "modified": modified,
        }));
    }

    serde_json::json!({
        "profile": profile_name,
        "timestamp": timestamp,
        "files": files,
    })
}

// ────────────────────────────────────────────────────────────────────
// Schedule helpers
// ────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleConfig {
    times: Vec<String>,
    interval_days: u32,
}

/// Compute the next run time from a schedule string.
/// Supports:
///   - New JSON format: `{"times": ["15:30","02:00"], "intervalDays": 1}`
///   - Legacy presets: "hourly", "daily", "every_6h", "weekly"
///   - Raw 5-field cron expressions
pub fn compute_next_run(schedule: Option<&str>) -> Option<String> {
    let sched = schedule?;
    if sched.is_empty() {
        return None;
    }

    // Try to parse as new JSON format first
    if sched.starts_with('{') {
        return compute_next_run_json(sched);
    }

    // Legacy preset / cron fallback
    let cron_expr = match sched {
        "hourly" => "0 * * * *",
        "daily" => "0 0 * * *",
        "every_6h" => "0 */6 * * *",
        "weekly" => "0 0 * * 0",
        raw => raw,
    };

    // cron crate requires 7-field expressions (sec min hour dom month dow year)
    // Convert our 5-field expressions by prepending "0" for seconds
    let full_cron = format!("0 {}", cron_expr);

    let parsed = cron::Schedule::from_str(&full_cron).ok()?;
    let next = parsed.upcoming(Local).next()?;
    Some(next.with_timezone(&Utc).to_rfc3339())
}

/// Parse the new JSON schedule format and find the next upcoming run time.
fn compute_next_run_json(json_str: &str) -> Option<String> {
    let config: ScheduleConfig = serde_json::from_str(json_str).ok()?;
    if config.times.is_empty() || config.interval_days == 0 {
        return None;
    }

    compute_next_run_json_in_timezone(&config, Utc::now(), &Local)
}

fn local_candidates<Tz: TimeZone>(
    timezone: &Tz,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Option<Vec<DateTime<Utc>>> {
    let local_time = date.and_hms_opt(hour, minute, 0)?;
    let mut candidates = match timezone.from_local_datetime(&local_time) {
        LocalResult::Single(value) => vec![value.with_timezone(&Utc)],
        // During the autumn DST fold, pick one canonical instant for this
        // wall-clock slot. Otherwise recomputing after the first occurrence
        // would schedule the same local time a second time.
        LocalResult::Ambiguous(first, second) => {
            let mut occurrences = vec![first.with_timezone(&Utc), second.with_timezone(&Utc)];
            occurrences.sort();
            occurrences.truncate(1);
            occurrences
        }
        // A wall-clock time inside the spring DST gap does not exist. Skip
        // that occurrence instead of silently running at a surprising hour.
        LocalResult::None => Vec::new(),
    };
    candidates.sort();
    Some(candidates)
}

fn compute_migrated_json_next_run<Tz: TimeZone>(
    json_str: &str,
    previous_next_run: Option<&str>,
    last_run_or_creation: Option<&str>,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> Option<String> {
    let config: ScheduleConfig = serde_json::from_str(json_str).ok()?;
    if config.times.is_empty() || config.interval_days == 0 {
        return None;
    }

    let lower_bound = last_run_or_creation
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));

    if let Some(previous) = previous_next_run
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
    {
        // The old implementation stored the configured wall-clock date and
        // time as though they were UTC. Translate that exact persisted
        // occurrence into the machine timezone. Using its date preserves
        // Every-N-days cadence; inventing an occurrence on the upgrade date
        // could run a weekly profile several days early.
        let persisted_date = previous.date_naive();
        let persisted_hour = previous.time().hour();
        let persisted_minute = previous.time().minute();
        let matches_configured_time = config.times.iter().any(|time_str| {
            let mut parts = time_str.split(':');
            let hour = parts.next().and_then(|value| value.parse::<u32>().ok());
            let minute = parts.next().and_then(|value| value.parse::<u32>().ok());
            parts.next().is_none()
                && hour == Some(persisted_hour)
                && minute == Some(persisted_minute)
        });
        if matches_configured_time {
            if let Some(translated) =
                local_candidates(timezone, persisted_date, persisted_hour, persisted_minute)?
                    .into_iter()
                    .next()
            {
                let occurred_after_last_run =
                    lower_bound.map(|bound| translated > bound).unwrap_or(true);
                if occurred_after_last_run {
                    // A past value intentionally remains overdue so the
                    // existing scheduler performs one catch-up; a future
                    // value keeps the exact eligible cadence date.
                    return Some(translated.to_rfc3339());
                }
            }
        }
    }

    compute_next_run_json_in_timezone(&config, now, timezone)
}

fn compute_next_run_json_in_timezone<Tz: TimeZone>(
    config: &ScheduleConfig,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> Option<String> {
    let today = now.with_timezone(timezone).date_naive();

    // Find the earliest upcoming time slot
    let mut best: Option<DateTime<Utc>> = None;

    for time_str in &config.times {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() != 2 {
            continue;
        }
        let hour: u32 = parts[0].parse().ok()?;
        let minute: u32 = parts[1].parse().ok()?;

        // Try today, then the configured day interval. The small loop also
        // skips a nonexistent DST wall-clock occurrence without getting a
        // scheduled profile permanently stuck.
        for occurrence in 0..=8_i64 {
            let day_offset = occurrence * config.interval_days as i64;
            let date = today + chrono::Duration::days(day_offset);
            let candidates = local_candidates(timezone, date, hour, minute)?;
            if let Some(candidate) = candidates.into_iter().find(|candidate| *candidate > now) {
                if best.map(|current| candidate < current).unwrap_or(true) {
                    best = Some(candidate);
                }
                break;
            }
        }
    }

    best.map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod schedule_time_tests {
    use super::*;
    use chrono_tz::Europe::Copenhagen;

    fn config(time: &str) -> ScheduleConfig {
        ScheduleConfig {
            times: vec![time.to_string()],
            interval_days: 1,
        }
    }

    #[test]
    fn copenhagen_machine_time_is_persisted_as_the_exact_summer_utc_instant() {
        let now = Copenhagen
            .with_ymd_and_hms(2026, 8, 19, 14, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            compute_next_run_json_in_timezone(&config("14:15"), now, &Copenhagen).as_deref(),
            Some("2026-08-19T12:15:00+00:00"),
        );
    }

    #[test]
    fn copenhagen_machine_time_uses_the_winter_utc_offset() {
        let now = Copenhagen
            .with_ymd_and_hms(2026, 1, 19, 14, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            compute_next_run_json_in_timezone(&config("14:15"), now, &Copenhagen).as_deref(),
            Some("2026-01-19T13:15:00+00:00"),
        );
    }

    #[test]
    fn nonexistent_spring_dst_time_is_skipped_to_the_next_valid_day() {
        let now = Copenhagen
            .with_ymd_and_hms(2026, 3, 29, 1, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            compute_next_run_json_in_timezone(&config("02:30"), now, &Copenhagen).as_deref(),
            Some("2026-03-30T00:30:00+00:00"),
        );
    }

    #[test]
    fn ambiguous_autumn_dst_time_uses_the_earliest_future_occurrence() {
        let now = DateTime::parse_from_rfc3339("2026-10-25T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            compute_next_run_json_in_timezone(&config("02:30"), now, &Copenhagen).as_deref(),
            Some("2026-10-25T00:30:00+00:00"),
        );
    }

    #[test]
    fn ambiguous_autumn_time_runs_only_once_per_local_date() {
        let now = DateTime::parse_from_rfc3339("2026-10-25T00:31:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            compute_next_run_json_in_timezone(&config("02:30"), now, &Copenhagen).as_deref(),
            Some("2026-10-26T01:30:00+00:00"),
        );
    }

    #[test]
    fn migration_keeps_a_newly_local_occurrence_overdue_for_one_catch_up() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:18:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = serde_json::json!({ "times": ["14:15"], "intervalDays": 1 }).to_string();
        assert_eq!(
            compute_migrated_json_next_run(
                &schedule,
                Some("2026-08-19T14:15:00Z"),
                Some("2026-08-18T10:00:00Z"),
                now,
                &Copenhagen,
            )
            .as_deref(),
            Some("2026-08-19T12:15:00+00:00"),
        );
    }

    #[test]
    fn migration_does_not_repeat_a_local_occurrence_that_already_ran() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:18:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = serde_json::json!({ "times": ["14:15"], "intervalDays": 1 }).to_string();
        assert_eq!(
            compute_migrated_json_next_run(
                &schedule,
                Some("2026-08-19T14:15:00Z"),
                Some("2026-08-19T12:16:00Z"),
                now,
                &Copenhagen,
            )
            .as_deref(),
            Some("2026-08-20T12:15:00+00:00"),
        );
    }

    #[test]
    fn migration_preserves_the_persisted_every_seven_days_date() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:18:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let schedule = serde_json::json!({ "times": ["14:15"], "intervalDays": 7 }).to_string();
        assert_eq!(
            compute_migrated_json_next_run(
                &schedule,
                Some("2026-08-25T14:15:00Z"),
                Some("2026-08-18T12:15:00Z"),
                now,
                &Copenhagen,
            )
            .as_deref(),
            Some("2026-08-25T12:15:00+00:00"),
        );
    }

    fn migration_profile(id: &str, next_run: &str) -> BackupProfile {
        BackupProfile {
            id: id.to_string(),
            owner_account: Some("owner@example.com".to_string()),
            name: id.to_string(),
            source_path: "C:\\test".to_string(),
            schedule: Some(
                serde_json::json!({ "times": ["14:15"], "intervalDays": 1 }).to_string(),
            ),
            retention: 0,
            folder: "/".to_string(),
            enabled: true,
            last_run: None,
            next_run: Some(next_run.to_string()),
            retry_count: 0,
            retry_at: None,
            last_error: None,
            last_error_code: None,
            schedule_state: "scheduled".to_string(),
            created_at: "2026-08-18T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn local_time_migration_is_transactional_and_marker_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(db::SCHEMA).unwrap();
        db::create_profile(
            &conn,
            &migration_profile("profile-a", "2026-08-19T14:15:00Z"),
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:18:00Z")
            .unwrap()
            .with_timezone(&Utc);

        migrate_schedule_times_in_timezone(&conn, now, &Copenhagen).unwrap();
        assert_eq!(
            db::get_profile(&conn, "profile-a")
                .unwrap()
                .next_run
                .as_deref(),
            Some("2026-08-19T12:15:00+00:00"),
        );
        assert_eq!(
            db::get_app_metadata(&conn, LOCAL_SCHEDULE_MIGRATION_KEY)
                .unwrap()
                .as_deref(),
            Some("done"),
        );

        conn.execute(
            "UPDATE backup_profiles SET next_run = '2030-01-01T00:00:00Z' WHERE id = 'profile-a'",
            [],
        )
        .unwrap();
        migrate_schedule_times_in_timezone(&conn, now, &Copenhagen).unwrap();
        assert_eq!(
            db::get_profile(&conn, "profile-a")
                .unwrap()
                .next_run
                .as_deref(),
            Some("2030-01-01T00:00:00Z"),
        );
    }

    #[test]
    fn failed_local_time_migration_rolls_back_profiles_and_marker() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(db::SCHEMA).unwrap();
        let old = "2026-08-19T14:15:00Z";
        db::create_profile(&conn, &migration_profile("profile-a", old)).unwrap();
        db::create_profile(&conn, &migration_profile("profile-b", old)).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_profile_b_migration
             BEFORE UPDATE ON backup_profiles
             WHEN NEW.id = 'profile-b'
             BEGIN
               SELECT RAISE(ABORT, 'injected migration failure');
             END;",
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:18:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert!(migrate_schedule_times_in_timezone(&conn, now, &Copenhagen).is_err());
        assert_eq!(
            db::get_profile(&conn, "profile-a")
                .unwrap()
                .next_run
                .as_deref(),
            Some(old)
        );
        assert_eq!(
            db::get_profile(&conn, "profile-b")
                .unwrap()
                .next_run
                .as_deref(),
            Some(old)
        );
        assert_eq!(
            db::get_app_metadata(&conn, LOCAL_SCHEDULE_MIGRATION_KEY).unwrap(),
            None,
        );
    }
}
