// ────────────────────────────────────────────────────────────────────
// Kopia engine integration (Phase 1)
//
// SaveState now acts as a UI + command wrapper around the Kopia backup
// engine. Kopia gives us content-defined deduplication, zstd compression,
// encrypted repositories, and policy-based retention natively. The agent
// never holds Backblaze provider credentials. For each operation it fetches
// short-lived, account-scoped credentials for SaveState's ciphertext-only
// repository gateway from `/repo/session`.
//
// Encryption stays user-controlled: the Kopia repository password is derived
// from the per-user master key, so backup data is encrypted client-side
// before it ever leaves the node.
// ────────────────────────────────────────────────────────────────────

use crate::api::{EngineJobReporter, RepoSession, SaveStateClient};
use crate::backup_operations::{self, AccountContext, BackupControl, BackupOperation};
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use tokio::sync::{Mutex as AsyncMutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

struct CachedBackupSession {
    session: RepoSession,
    account_scope: String,
    session_generation: u64,
    valid_until: Instant,
}

struct CachedRetention {
    account_scope: String,
    session_generation: u64,
    keep_latest: u32,
}

static BACKUP_SESSION: OnceLock<Mutex<Option<CachedBackupSession>>> = OnceLock::new();
static REPOSITORY_CONNECT_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
static CANCELLED_RESTORES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static MANIFEST_UPDATE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static LAST_RETENTION: OnceLock<Mutex<Option<CachedRetention>>> = OnceLock::new();
static OPERATION_GATE: OnceLock<RwLock<()>> = OnceLock::new();
static CLEANUP_RUNNING: AtomicBool = AtomicBool::new(false);
static SESSION_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

struct LastCleanup {
    account_scope: String,
    completed_at: Instant,
}

static LAST_CLEANUP: OnceLock<Mutex<Option<LastCleanup>>> = OnceLock::new();

fn backup_session_cache() -> &'static Mutex<Option<CachedBackupSession>> {
    BACKUP_SESSION.get_or_init(|| Mutex::new(None))
}

fn repository_connect_lock() -> &'static AsyncMutex<()> {
    REPOSITORY_CONNECT_LOCK.get_or_init(|| AsyncMutex::new(()))
}

fn cancelled_restores() -> &'static Mutex<HashSet<String>> {
    CANCELLED_RESTORES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn manifest_update_lock() -> &'static tokio::sync::Mutex<()> {
    MANIFEST_UPDATE_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn last_retention() -> &'static Mutex<Option<CachedRetention>> {
    LAST_RETENTION.get_or_init(|| Mutex::new(None))
}

fn operation_gate() -> &'static RwLock<()> {
    OPERATION_GATE.get_or_init(|| RwLock::new(()))
}

fn last_cleanup() -> &'static Mutex<Option<LastCleanup>> {
    LAST_CLEANUP.get_or_init(|| Mutex::new(None))
}

fn cleanup_within_cooldown(account_scope: &str) -> bool {
    last_cleanup()
        .lock()
        .map(|last| {
            last.as_ref().is_some_and(|cleanup| {
                cleanup.account_scope == account_scope
                    && cleanup.completed_at.elapsed() < Duration::from_secs(30 * 60)
            })
        })
        .unwrap_or(false)
}

fn mark_cleanup_complete(account_scope: &str) {
    if let Ok(mut last) = last_cleanup().lock() {
        *last = Some(LastCleanup {
            account_scope: account_scope.to_string(),
            completed_at: Instant::now(),
        });
    }
}

/// Hold a shared lease for any backup-engine operation that must finish before
/// an application update may install. Multiple normal operations can coexist,
/// while the updater requires the exclusive side of the same gate.
/// A capability for an already admitted engine workflow. Pass it to nested
/// operations instead of reacquiring a writer-preferring lock.
pub struct EngineLease<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

fn begin_operation_in(gate: &RwLock<()>) -> Result<EngineLease<'_>> {
    gate.try_read().map(|guard| EngineLease { _guard: guard }).map_err(|_| {
        anyhow!("The engine is busy with maintenance, an account change, or an update; try again when it finishes")
    })
}

pub async fn begin_operation() -> Result<EngineLease<'static>> {
    // New work cannot queue captured old account state behind a session change.
    begin_operation_in(operation_gate())
}

/// Reserve the engine exclusively for an update. This deliberately does not
/// wait: clicking Update while a backup or restore is active should leave that
/// operation untouched and tell the user to try again when it has finished.
pub fn try_begin_update() -> Result<RwLockWriteGuard<'static, ()>> {
    operation_gate()
        .try_write()
        .map_err(|_| anyhow!("A backup, restore, deletion, or maintenance task is still running"))
}

pub fn clear_session_cache() {
    // A repository warm-up may still be completing while the user signs out
    // or switches accounts. Invalidating the generation prevents that stale
    // operation from repopulating the cache after it has been cleared.
    SESSION_CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut cache) = backup_session_cache().lock() {
        *cache = None;
    }
    if let Ok(mut retention) = last_retention().lock() {
        *retention = None;
    }
}

pub fn cancel_restore(snapshot_id: &str) {
    if let Ok(mut cancelled) = cancelled_restores().lock() {
        cancelled.insert(snapshot_id.to_string());
    }
}

fn clear_restore_cancellation(snapshot_id: &str) {
    if let Ok(mut cancelled) = cancelled_restores().lock() {
        cancelled.remove(snapshot_id);
    }
}

fn restore_is_cancelled(snapshot_id: &str) -> bool {
    cancelled_restores()
        .lock()
        .map(|cancelled| cancelled.contains(snapshot_id))
        .unwrap_or(false)
}

fn ensure_restore_not_cancelled(snapshot_id: &str) -> Result<()> {
    if restore_is_cancelled(snapshot_id) {
        Err(anyhow!("Restore cancelled"))
    } else {
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────
// Progress event payload (mirrors backup.rs::BackupProgress shape)
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EngineProgress {
    pub id: String,
    pub stage: String,
    pub progress: f64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageCleanupProgress {
    pub status: String,
    pub message: String,
}

fn emit_storage_cleanup(app: &tauri::AppHandle, status: &str, message: &str) {
    let _ = app.emit(
        "storage-cleanup",
        StorageCleanupProgress {
            status: status.to_string(),
            message: message.to_string(),
        },
    );
}

fn emit_progress(app: &tauri::AppHandle, id: &str, stage: &str, progress: f64, message: &str) {
    let _ = app.emit(
        "backup-progress",
        EngineProgress {
            id: id.to_string(),
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
        },
    );
}

fn emit_restore_progress(
    app: &tauri::AppHandle,
    id: &str,
    stage: &str,
    progress: f64,
    message: &str,
) {
    let _ = app.emit(
        "restore-progress",
        EngineProgress {
            id: id.to_string(),
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
        },
    );
}

enum ProgressChannel {
    Backup,
    Restore,
}

/// Guarantees that every visible operation reaches a terminal UI state, even
/// when an early `?` returns during repository setup or authorization.
struct TerminalProgressGuard<'a> {
    app: &'a tauri::AppHandle,
    id: &'a str,
    channel: ProgressChannel,
    armed: bool,
}

impl<'a> TerminalProgressGuard<'a> {
    fn backup(app: &'a tauri::AppHandle, id: &'a str) -> Self {
        Self {
            app,
            id,
            channel: ProgressChannel::Backup,
            armed: true,
        }
    }

    fn restore(app: &'a tauri::AppHandle, id: &'a str) -> Self {
        Self {
            app,
            id,
            channel: ProgressChannel::Restore,
            armed: true,
        }
    }

    fn finish(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalProgressGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.channel {
            ProgressChannel::Backup => emit_progress(
                self.app,
                self.id,
                "error",
                0.0,
                "Backup failed. Check the error message and try again.",
            ),
            ProgressChannel::Restore => emit_restore_progress(
                self.app,
                self.id,
                "error",
                0.0,
                "Restore failed. Check the error message and try again.",
            ),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Snapshot summary returned to the UI
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KopiaSnapshot {
    pub id: String,
    pub source_path: String,
    pub start_time: String,
    pub size: u64,
    pub file_count: u64,
    #[serde(default = "root_folder")]
    pub folder: String,
    #[serde(default = "file_backup_kind")]
    pub backup_kind: String,
    #[serde(default)]
    pub database_profile_id: Option<String>,
    #[serde(default)]
    pub database_profile_name: Option<String>,
    #[serde(default)]
    pub root_object_id: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub profile_name: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub version_number: Option<u64>,
}

fn root_folder() -> String {
    "/".to_string()
}

fn file_backup_kind() -> String {
    "files".to_string()
}

pub struct StreamSourceCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

pub struct StreamRestoreCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseProgress {
    profile_id: String,
    stage: String,
    progress: f64,
    message: String,
}

fn emit_database_progress(
    app: &tauri::AppHandle,
    profile_id: &str,
    stage: &str,
    progress: f64,
    message: &str,
) {
    let _ = app.emit(
        "database-progress",
        DatabaseProgress {
            profile_id: profile_id.to_string(),
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
        },
    );
}

fn emit_database_restore_progress(
    app: &tauri::AppHandle,
    profile_id: &str,
    stage: &str,
    progress: f64,
    message: &str,
) {
    let _ = app.emit(
        "database-restore-progress",
        DatabaseProgress {
            profile_id: profile_id.to_string(),
            stage: stage.to_string(),
            progress,
            message: message.to_string(),
        },
    );
}

// ────────────────────────────────────────────────────────────────────
// Binary + paths
// ────────────────────────────────────────────────────────────────────

/// Resolve the `kopia` executable. Resolution order:
///   1. `SAVESTATE_KOPIA_BIN` env override
///   2. Bundled sidecar in the Tauri resource directory
///   3. `kopia` on the system PATH
fn kopia_binary(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("SAVESTATE_KOPIA_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }

    let exe = if cfg!(windows) { "kopia.exe" } else { "kopia" };

    if let Ok(resource_dir) = app.path().resource_dir() {
        let candidate = resource_dir.join("bin").join(exe);
        if candidate.exists() {
            return candidate;
        }
        let candidate = resource_dir.join(exe);
        if candidate.exists() {
            return candidate;
        }
    }

    PathBuf::from(exe)
}

/// Per-app isolated Kopia config + cache directory.
fn kopia_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("SaveState")
        .join("kopia")
}

fn repository_data_dir(session: &RepoSession) -> PathBuf {
    let bucket_slug: String = session
        .bucket
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(session.endpoint.as_bytes());
    hasher.update([0]);
    hasher.update(session.bucket.as_bytes());
    hasher.update([0]);
    hasher.update(session.prefix.as_bytes());
    let fingerprint = hex::encode(hasher.finalize());
    let repository_id = format!(
        "{}-{}",
        if bucket_slug.is_empty() {
            "repository"
        } else {
            &bucket_slug
        },
        &fingerprint[..20],
    );
    kopia_data_dir().join("repositories").join(repository_id)
}

fn kopia_config_file(session: &RepoSession) -> PathBuf {
    repository_data_dir(session).join("repository.config")
}

fn kopia_cache_dir(session: &RepoSession) -> PathBuf {
    repository_data_dir(session).join("cache")
}

// ────────────────────────────────────────────────────────────────────
// Command runner
// ────────────────────────────────────────────────────────────────────

/// Run a kopia subcommand. `repo_password` is injected via `KOPIA_PASSWORD`
/// and `session` (when present) supplies repository-gateway credentials via
/// standard AWS env vars so secrets never appear in the process argument list.
fn build_kopia_command(
    app: &tauri::AppHandle,
    args: &[String],
    repo_password: Option<&str>,
    session: Option<&RepoSession>,
) -> Command {
    let bin = kopia_binary(app);
    let repository_dir = session
        .map(repository_data_dir)
        .unwrap_or_else(kopia_data_dir);
    let config_file = session
        .map(kopia_config_file)
        .unwrap_or_else(|| repository_dir.join("repository.config"));
    let cache_dir = session
        .map(kopia_cache_dir)
        .unwrap_or_else(|| repository_dir.join("cache"));

    std::fs::create_dir_all(&repository_dir).ok();

    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--config-file").arg(&config_file);
    cmd.args(args);

    if let Some(pw) = repo_password {
        cmd.env("KOPIA_PASSWORD", pw);
    }
    cmd.env("KOPIA_CACHE_DIRECTORY", &cache_dir);

    if let Some(s) = session {
        cmd.env("AWS_ACCESS_KEY_ID", &s.access_key_id);
        cmd.env("AWS_SECRET_ACCESS_KEY", &s.secret_access_key);
        cmd.env("AWS_REGION", &s.region);
    }

    // Avoid spawning a visible console window on Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd
}

fn run_kopia(
    app: &tauri::AppHandle,
    args: &[String],
    repo_password: Option<&str>,
    session: Option<&RepoSession>,
) -> Result<Output> {
    let bin = kopia_binary(app);
    let mut cmd = build_kopia_command(app, args, repo_password, session);

    cmd.output()
        .with_context(|| format!("Failed to execute kopia at {:?}", bin))
}

fn run_kopia_for_backup(
    app: &tauri::AppHandle,
    args: &[String],
    repo_password: Option<&str>,
    session: Option<&RepoSession>,
    cancellation: Option<&Arc<BackupControl>>,
) -> Result<Output> {
    let Some(control) = cancellation else {
        return run_kopia(app, args, repo_password, session);
    };
    if control.is_cancel_requested() {
        return Err(backup_operations::cancelled_error());
    }

    let bin = kopia_binary(app);
    let mut cmd = build_kopia_command(app, args, repo_password, session);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute kopia at {:?}", bin))?;
    let mut stdout = child
        .stdout
        .take()
        .context("Failed to capture kopia stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Failed to capture kopia stderr")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stderr.read_to_end(&mut output);
        output
    });

    let status = loop {
        if control.is_cancel_requested() {
            if let Err(error) = child.kill() {
                if let Some(status) = child
                    .try_wait()
                    .context("Failed to re-check Kopia after cancellation")?
                {
                    break status;
                }
                let message = format!("Failed to stop Kopia process: {}", error);
                control.record_cancellation_error(message.clone());
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(anyhow!("BACKUP_CANCEL_FAILED: {}", message));
            }
            let status = child
                .wait()
                .context("Failed to wait for stopped Kopia backup")?;
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(anyhow!(
                "BACKUP_CANCELLED: Kopia backup stopped (process status: {})",
                status
            ));
        }
        if let Some(status) = child.try_wait().context("Failed to poll Kopia backup")? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("Failed to collect kopia stdout"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Failed to collect kopia stderr"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn run_kopia_cancellable(
    app: &tauri::AppHandle,
    args: &[String],
    repo_password: Option<&str>,
    session: Option<&RepoSession>,
    snapshot_id: &str,
) -> Result<Output> {
    let bin = kopia_binary(app);
    let mut cmd = build_kopia_command(app, args, repo_password, session);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute kopia at {:?}", bin))?;

    let mut stdout = child
        .stdout
        .take()
        .context("Failed to capture kopia stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Failed to capture kopia stderr")?;
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stderr.read_to_end(&mut output);
        output
    });

    let status = loop {
        if restore_is_cancelled(snapshot_id) {
            let _ = child.kill();
            let status = child.wait().context("Failed to stop kopia restore")?;
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(anyhow!("Restore cancelled (process status: {})", status));
        }
        if let Some(status) = child.try_wait().context("Failed to poll kopia restore")? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("Failed to collect kopia stdout"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Failed to collect kopia stderr"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn ensure_success(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(classify_kopia_error(action, &stderr))
}

fn repository_is_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("repository not initialized")
        || lower.contains("repository is not initialized")
        || lower.contains("no repository found")
}

fn classify_kopia_error(action: &str, stderr: &str) -> anyhow::Error {
    let normalized = stderr.replace(['\r', '\n', '\t'], " ");
    let lower = normalized.to_ascii_lowercase();
    eprintln!("Kopia {} failed: {}", action, normalized.trim());

    if lower.contains("message authentication failed")
        || lower.contains("unable to decrypt content")
        || lower.contains("invalid checksum")
        || lower.contains("unable to load manifest content")
    {
        return anyhow!(
            "REPOSITORY_KEY_MISMATCH: This backup repository cannot be decrypted with the current account key. No backup data was changed."
        );
    }
    if lower.contains("designated user") {
        return anyhow!(
            "MAINTENANCE_OWNER_MISMATCH: Storage cleanup is waiting for repository ownership to be updated"
        );
    }

    let concise = normalized.trim();
    let concise = if concise.chars().count() > 240 {
        format!("{}…", concise.chars().take(240).collect::<String>())
    } else {
        concise.to_string()
    };
    anyhow!("Kopia {} failed: {}", action, concise)
}

// ────────────────────────────────────────────────────────────────────
// Repository lifecycle
// ────────────────────────────────────────────────────────────────────

fn api_from_state(state: &AppStateWrapper) -> Result<SaveStateClient> {
    let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
    Ok(guard.api.clone())
}

/// Build the shared `s3` connection arguments for connect/create.
fn s3_connect_args(session: &RepoSession) -> Vec<String> {
    let endpoint = session.endpoint_host.clone().unwrap_or_else(|| {
        session
            .endpoint
            .replace("https://", "")
            .replace("http://", "")
    });

    vec![
        "s3".to_string(),
        format!("--bucket={}", session.bucket),
        format!("--endpoint={}", endpoint),
        format!("--prefix={}", session.prefix),
        format!("--region={}", session.region),
    ]
}

fn backup_reliability_policy_args(
    new_repository: bool,
    enable_volume_shadow_copy: bool,
) -> Option<Vec<String>> {
    let mut args = vec![
        "policy".to_string(),
        "set".to_string(),
        "--global".to_string(),
        // Older clients used one global keep-latest value for every profile.
        // Keep Kopia's native expiration effectively unlimited and let the
        // client prune only versions that still live in each managed folder.
        // This is the largest value accepted by Kopia's signed counter.
        "--keep-latest=2147483647".to_string(),
        "--keep-hourly=0".to_string(),
        "--keep-daily=0".to_string(),
        "--keep-weekly=0".to_string(),
        "--keep-monthly=0".to_string(),
        "--keep-annual=0".to_string(),
    ];
    if new_repository {
        args.push("--compression=zstd".to_string());
    }
    if enable_volume_shadow_copy {
        // Kopia's native VSS integration snapshots a stable Windows volume
        // view before it reads files that may be locked by running services.
        // `when-available` falls back to normal traversal without prompting for
        // elevation if this process cannot create a shadow copy. Read errors
        // remain fatal, so the fallback cannot silently produce a partial backup.
        args.push("--enable-volume-shadow-copy=when-available".to_string());
    }
    Some(args)
}

/// Connect to the repository, creating it first if it does not yet exist
/// (only in backup mode, where we hold write capability).
async fn ensure_repo(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    mode: &str,
    restore_grant_id: Option<&str>,
) -> Result<(RepoSession, String)> {
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    ensure_repo_with_context(app, &context, mode, restore_grant_id, None).await
}

async fn ensure_repo_with_context(
    app: &tauri::AppHandle,
    context: &AccountContext,
    mode: &str,
    restore_grant_id: Option<&str>,
    cancellation: Option<&Arc<BackupControl>>,
) -> Result<(RepoSession, String)> {
    let password = context.repository_password.clone();
    let cacheable_backup_session = mode == "backup" && restore_grant_id.is_none();

    // Reuse a recently connected backup session in memory. This removes one
    // API round trip and one Kopia repository-connect process from consecutive
    // backup, retention, and delete operations while respecting credential
    // expiry. Restore grants remain one-time and are never cached.
    if cacheable_backup_session {
        if let Ok(cache) = backup_session_cache().lock() {
            if let Some(cached) = cache.as_ref() {
                if cached.account_scope == context.account_scope
                    && cached.session_generation == context.session_generation
                    && Instant::now() < cached.valid_until
                {
                    return Ok((cached.session.clone(), password));
                }
            }
        }
    }

    // App-start warm-up and an immediately requested backup can arrive at the
    // same time. Serialize backup repository connections, then check the cache
    // again so only one API session and one Kopia connect process are needed.
    let _connect_guard = if cacheable_backup_session {
        if let Some(control) = cancellation {
            Some(tokio::select! {
                guard = repository_connect_lock().lock() => guard,
                _ = control.wait_cancelled() => return Err(backup_operations::cancelled_error()),
            })
        } else {
            Some(repository_connect_lock().lock().await)
        }
    } else {
        None
    };
    if cacheable_backup_session {
        if let Ok(cache) = backup_session_cache().lock() {
            if let Some(cached) = cache.as_ref() {
                if cached.account_scope == context.account_scope
                    && cached.session_generation == context.session_generation
                    && Instant::now() < cached.valid_until
                {
                    return Ok((cached.session.clone(), password));
                }
            }
        }
    }

    let cache_generation = SESSION_CACHE_GENERATION.load(Ordering::SeqCst);

    let session = if let Some(control) = cancellation {
        tokio::select! {
            session = context.api.repo_session(mode, restore_grant_id) => session?,
            _ = control.wait_cancelled() => return Err(backup_operations::cancelled_error()),
        }
    } else {
        context.api.repo_session(mode, restore_grant_id).await?
    };
    if cancellation
        .map(|control| control.is_cancel_requested())
        .unwrap_or(false)
    {
        return Err(backup_operations::cancelled_error());
    }

    let app = app.clone();
    let session_cloned = session.clone();
    let password_cloned = password.clone();
    let mode_is_backup = mode == "backup";
    let cancellation = cancellation.cloned();

    // Kopia CLI calls are blocking; run them off the async runtime.
    let session_for_blocking = session_cloned.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let base = s3_connect_args(&session_for_blocking);

        // Try to connect to an existing repository.
        let mut connect = vec!["repository".to_string(), "connect".to_string()];
        connect.extend(base.clone());
        let out = run_kopia_for_backup(
            &app,
            &connect,
            Some(&password_cloned),
            Some(&session_for_blocking),
            cancellation.as_ref(),
        )?;

        let mut new_repository = false;
        if !out.status.success() {
            let connect_error = String::from_utf8_lossy(&out.stderr);

            // Creating over an existing repository after an authentication,
            // decryption, or network failure can make recovery harder. Only
            // initialize storage when Kopia explicitly reports that no
            // repository exists there yet.
            if mode_is_backup && repository_is_missing(&connect_error) {
                let mut create = vec!["repository".to_string(), "create".to_string()];
                create.extend(base);
                let out = run_kopia_for_backup(
                    &app,
                    &create,
                    Some(&password_cloned),
                    Some(&session_for_blocking),
                    cancellation.as_ref(),
                )?;
                ensure_success(&out, "repository create")?;
                new_repository = true;
            } else {
                return Err(classify_kopia_error("repository connect", &connect_error));
            }
        }

        if mode_is_backup {
            if let Some(policy) =
                backup_reliability_policy_args(new_repository, cfg!(target_os = "windows"))
            {
                // Reapply the idempotent policy after connecting so repositories
                // created by older SaveState versions gain Windows VSS support as
                // soon as the updated client performs its first backup warm-up.
                let out = run_kopia_for_backup(
                    &app,
                    &policy,
                    Some(&password_cloned),
                    Some(&session_for_blocking),
                    cancellation.as_ref(),
                )?;
                ensure_success(&out, "backup reliability policy")?;
            }
        }

        Ok(())
    })
    .await
    .context("kopia connect task panicked")??;

    if cacheable_backup_session
        && SESSION_CACHE_GENERATION.load(Ordering::SeqCst) == cache_generation
    {
        let cache_lifetime = Duration::from_secs(session.expires_in.saturating_sub(30).max(1));
        if let Ok(mut cache) = backup_session_cache().lock() {
            *cache = Some(CachedBackupSession {
                session: session.clone(),
                account_scope: context.account_scope.clone(),
                session_generation: context.session_generation,
                valid_until: Instant::now() + cache_lifetime,
            });
        }
    }

    Ok((session, password))
}

// ────────────────────────────────────────────────────────────────────
// Operations
// ────────────────────────────────────────────────────────────────────

/// Create a deduplicated, compressed snapshot of one or more paths.
pub async fn backup_paths(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    paths: Vec<String>,
) -> Result<String> {
    backup_paths_with_trigger(app, state, paths, "manual", "/").await
}

pub async fn backup_paths_in_folder(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    paths: Vec<String>,
    folder: &str,
) -> Result<String> {
    backup_paths_with_trigger(app, state, paths, "manual", folder).await
}

pub async fn backup_paths_with_trigger(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    paths: Vec<String>,
    trigger: &'static str,
    folder: &str,
) -> Result<String> {
    let display_name = if paths.len() == 1 {
        std::path::Path::new(&paths[0])
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("Quick backup")
            .to_string()
    } else {
        format!("{} selected files", paths.len())
    };
    let operation = backup_operations::begin(state, display_name)?;
    prepare_repository_for_backup(app, &operation).await?;
    let engine = begin_operation().await?;
    let result =
        backup_paths_with_operation(app, &engine, &operation, paths, trigger, folder, None, None)
            .await;
    operation.finish_tracking().await;
    result
}

pub async fn backup_paths_with_operation(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    operation: &BackupOperation,
    paths: Vec<String>,
    trigger: &'static str,
    folder: &str,
    profile: Option<(&str, &str)>,
    keep_latest: Option<usize>,
) -> Result<String> {
    if paths.is_empty() {
        return Err(anyhow!("No backup paths provided"));
    }
    let folder = normalize_snapshot_folder(folder)?;
    let profile = profile.map(|(id, name)| (id.to_string(), name.to_string()));

    let op_id = uuid::Uuid::new_v4().to_string();
    let mut engine_job = EngineJobReporter::start_backup(
        operation.context.api.clone(),
        op_id.clone(),
        trigger,
        operation.account_scope().to_string(),
    );
    let mut terminal_progress = TerminalProgressGuard::backup(app, &op_id);
    emit_progress(app, &op_id, "compressing", 0.1, "Connecting to repository…");

    let result: Result<KopiaSnapshot> = async {
        operation.ensure_not_cancelled()?;
        engine_job.progress("repository_connect");
        let (session, password) = ensure_repo_with_context(
            app, &operation.context, "backup", None, Some(&operation.control),
        ).await?;
        operation.ensure_not_cancelled()?;

        emit_progress(app, &op_id, "uploading", 0.3, "Scanning and deduplicating…");
        engine_job.progress("snapshot_create");
        let app_c = app.clone();
        let session_c = session.clone();
        let control = Arc::clone(&operation.control);
        let profile_c = profile.clone();
        let trigger_c = trigger.to_string();
        let mut snapshot = tokio::task::spawn_blocking(move || -> Result<KopiaSnapshot> {
            let mut args = vec!["snapshot".to_string(), "create".to_string()];
            args.extend(paths);
            args.push("--tags=backup-kind:files".to_string());
            args.push(format!("--tags=savestate-trigger:{trigger_c}"));
            if let Some((profile_id, _)) = profile_c.as_ref() {
                args.push(format!("--tags=savestate-profile:{profile_id}"));
            }
            args.push("--json".to_string());
            args.push("--no-progress".to_string());
            let out = run_kopia_for_backup(
                &app_c, &args, Some(&password), Some(&session_c), Some(&control),
            )?;
            ensure_success(&out, "snapshot create")?;
            let value: serde_json::Value = serde_json::from_str(
                String::from_utf8_lossy(&out.stdout).trim(),
            ).context("Kopia returned invalid snapshot JSON")?;
            let snapshot = parse_snapshot(&value);
            if snapshot.id.is_empty() {
                return Err(anyhow!("Kopia snapshot metadata was incomplete"));
            }
            Ok(snapshot)
        }).await.context("kopia snapshot task panicked")??;
        snapshot.folder = folder;
        snapshot.trigger = Some(trigger.to_string());
        if let Some((profile_id, profile_name)) = profile.as_ref() {
            snapshot.profile_id = Some(profile_id.clone());
            snapshot.profile_name = Some(profile_name.clone());
        }

        if let Err(cancelled) = operation.ensure_not_cancelled() {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, false).await?;
            return Err(cancelled);
        }

        engine_job.progress("manifest_sync");
        let retention = profile
            .as_ref()
            .and_then(|(profile_id, _)| keep_latest.map(|keep| (profile_id.as_str(), keep)));
        if let Err(error) =
            upsert_manifest_snapshot(&operation.context.api, snapshot.clone(), retention).await
        {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, true).await?;
            return Err(error.context("Snapshot creation was rolled back because its server manifest could not be synchronized"));
        }
        if let Err(cancelled) = operation.control.mark_committed().await {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, true).await?;
            return Err(cancelled);
        }
        Ok(snapshot)
    }.await;

    match result {
        Ok(snapshot) => {
            emit_progress(app, &op_id, "done", 1.0, "Backup complete");
            terminal_progress.finish();
            engine_job.finish(
                "succeeded",
                "completed",
                Some(snapshot.size),
                Some(snapshot.file_count),
                None,
            );
            if operation
                .api()
                .enforce_retention()
                .await
                .map(|result| result.maintenance_recommended)
                .unwrap_or(false)
            {
                schedule_storage_cleanup(app.clone());
            }
            Ok(snapshot.id)
        }
        Err(error) if backup_operations::is_cancelled(&error) => {
            emit_progress(
                app,
                &op_id,
                "cancelled",
                0.0,
                "Backup stopped during sign-out",
            );
            terminal_progress.finish();
            engine_job.finish("cancelled", "cancelled", None, None, Some("user_logout"));
            Err(error)
        }
        Err(error) => {
            engine_job.fail_with_error("backup_failed", &error, None, None);
            // Send the actionable failure chain as the terminal event. The
            // command rejection carries the same detail, so the UI de-duplicates
            // manual runs while scheduled runs still receive a useful message.
            emit_progress(app, &op_id, "error", 0.0, &format!("{error:#}"));
            terminal_progress.finish();
            Err(error)
        }
    }
}

async fn rollback_uncommitted_snapshot(
    app: &tauri::AppHandle,
    operation: &BackupOperation,
    session: &RepoSession,
    snapshot_id: &str,
    manifest_may_exist: bool,
) -> Result<()> {
    let app = app.clone();
    let session = session.clone();
    let password = operation.context.repository_password.clone();
    let snapshot_id_owned = snapshot_id.to_string();
    let result = execute_rollback_steps(
        manifest_may_exist,
        || async move {
            let cleanup = tokio::task::spawn_blocking(move || -> Result<()> {
                let args = vec![
                    "snapshot".to_string(), "delete".to_string(), snapshot_id_owned,
                    "--delete".to_string(),
                ];
                let output = run_kopia(&app, &args, Some(&password), Some(&session))?;
                if output.status.success() { return Ok(()); }
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ).to_ascii_lowercase();
                if combined.contains("no snapshots matched") { return Ok(()); }
                ensure_success(&output, "cancelled snapshot delete")
            }).await.context("cancelled snapshot cleanup task panicked")?;
            cleanup.context("Could not remove cancelled Kopia snapshot safely")
        },
        || async {
            remove_manifest_snapshot(&operation.context.api, snapshot_id)
                .await
                .context("Kopia removed the cancelled snapshot, but its metadata cleanup must be retried")
        },
    ).await;

    if let Err(error) = result {
        let message = error.to_string();
        if operation.control.is_cancel_requested() {
            operation.control.record_cancellation_error(message.clone());
        }
        return Err(anyhow!("BACKUP_CANCEL_FAILED: {}", message));
    }
    Ok(())
}

async fn execute_rollback_steps<Delete, DeleteFuture, Remove, RemoveFuture>(
    manifest_may_exist: bool,
    delete_snapshot: Delete,
    remove_manifest: Remove,
) -> Result<()>
where
    Delete: FnOnce() -> DeleteFuture,
    DeleteFuture: std::future::Future<Output = Result<()>>,
    Remove: FnOnce() -> RemoveFuture,
    RemoveFuture: std::future::Future<Output = Result<()>>,
{
    // Kopia deletion always comes first. A failure leaves the manifest intact,
    // so the retained snapshot remains discoverable and can be retried.
    delete_snapshot().await?;
    if manifest_may_exist {
        remove_manifest().await?;
    }
    Ok(())
}

pub(crate) struct StreamPipelineOutput {
    pub(crate) source_status: std::process::ExitStatus,
    pub(crate) source_stderr: Vec<u8>,
    pub(crate) kopia_output: Output,
    pub(crate) source_bytes: u64,
}

fn run_stream_pipeline(
    app: &tauri::AppHandle,
    source: StreamSourceCommand,
    kopia_args: &[String],
    password: &str,
    session: &RepoSession,
    control: &Arc<BackupControl>,
) -> Result<StreamPipelineOutput> {
    let kopia_command = build_kopia_command(app, kopia_args, Some(password), Some(session));
    run_stream_pipeline_commands(source, kopia_command, control)
}

/// Command boundary shared with disposable local integration tests; no app,
/// account credentials or global repository configuration are needed.
pub(crate) fn run_stream_pipeline_commands(
    source: StreamSourceCommand,
    mut kopia_command: Command,
    control: &Arc<BackupControl>,
) -> Result<StreamPipelineOutput> {
    let mut source_command = Command::new(&source.program);
    source_command
        .args(&source.args)
        .envs(source.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        source_command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut source_child = source_command.spawn().with_context(|| {
        format!(
            "Failed to start database export tool at {}",
            source.program.display()
        )
    })?;

    kopia_command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut kopia_child = match kopia_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = source_child.kill();
            let _ = source_child.wait();
            return Err(error).context("Failed to start the encrypted database snapshot");
        }
    };

    let mut source_stdout = source_child
        .stdout
        .take()
        .context("Failed to capture database export output")?;
    let mut source_stderr = source_child
        .stderr
        .take()
        .context("Failed to capture database export errors")?;
    let mut kopia_stdin = kopia_child
        .stdin
        .take()
        .context("Failed to open the encrypted snapshot stream")?;
    let mut kopia_stdout = kopia_child
        .stdout
        .take()
        .context("Failed to capture Kopia output")?;
    let mut kopia_stderr = kopia_child
        .stderr
        .take()
        .context("Failed to capture Kopia errors")?;

    let transferred = Arc::new(AtomicU64::new(0));
    let transferred_writer = Arc::clone(&transferred);
    let stream_thread = std::thread::spawn(move || -> std::io::Result<()> {
        let copied = std::io::copy(&mut source_stdout, &mut kopia_stdin)?;
        transferred_writer.store(copied, Ordering::Release);
        Ok(())
    });
    let source_error_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = source_stderr.read_to_end(&mut output);
        output
    });
    let kopia_output_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = kopia_stdout.read_to_end(&mut output);
        output
    });
    let kopia_error_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = kopia_stderr.read_to_end(&mut output);
        output
    });

    let mut source_status = None;
    let mut kopia_status = None;
    loop {
        if control.is_cancel_requested() {
            let source_kill = source_child.kill();
            let kopia_kill = kopia_child.kill();
            let _ = source_child.wait();
            let _ = kopia_child.wait();
            let _ = stream_thread.join();
            let _ = source_error_thread.join();
            let _ = kopia_output_thread.join();
            let _ = kopia_error_thread.join();
            if let Err(error) = source_kill.and(kopia_kill) {
                let message = format!("Failed to stop database backup processes: {error}");
                control.record_cancellation_error(message.clone());
                return Err(anyhow!("BACKUP_CANCEL_FAILED: {message}"));
            }
            return Err(backup_operations::cancelled_error());
        }

        if source_status.is_none() {
            source_status = source_child
                .try_wait()
                .context("Failed to poll database export")?;
        }
        if kopia_status.is_none() {
            kopia_status = kopia_child
                .try_wait()
                .context("Failed to poll encrypted database snapshot")?;
        }

        if source_status.is_some_and(|status| !status.success()) && kopia_status.is_none() {
            let _ = kopia_child.kill();
        }
        if kopia_status.is_some_and(|status| !status.success()) && source_status.is_none() {
            let _ = source_child.kill();
        }
        if source_status.is_some() && kopia_status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let stream_result = stream_thread
        .join()
        .map_err(|_| anyhow!("Database stream worker stopped unexpectedly"))?;
    let source_stderr = source_error_thread
        .join()
        .map_err(|_| anyhow!("Failed to collect database export errors"))?;
    let kopia_stdout = kopia_output_thread
        .join()
        .map_err(|_| anyhow!("Failed to collect Kopia output"))?;
    let kopia_stderr = kopia_error_thread
        .join()
        .map_err(|_| anyhow!("Failed to collect Kopia errors"))?;

    if let Err(error) = stream_result {
        if source_status.is_some_and(|status| status.success())
            && kopia_status.is_some_and(|status| status.success())
        {
            return Err(error).context("Database export stream was interrupted");
        }
    }

    Ok(StreamPipelineOutput {
        source_status: source_status.context("Database export process did not exit")?,
        source_stderr,
        kopia_output: Output {
            status: kopia_status.context("Kopia process did not exit")?,
            stdout: kopia_stdout,
            stderr: kopia_stderr,
        },
        source_bytes: transferred.load(Ordering::Acquire),
    })
}

/// Stream a command's stdout directly into a Kopia snapshot. Database
/// credentials remain in the child environment and plaintext SQL is never
/// materialized as a local file.
pub async fn backup_stream_with_operation(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    operation: &BackupOperation,
    source: StreamSourceCommand,
    stdin_filename: String,
    profile_id: &str,
    profile_name: &str,
    trigger: &'static str,
    folder: &str,
    keep_latest: Option<usize>,
) -> Result<String> {
    let folder = normalize_snapshot_folder(folder)?;
    let op_id = uuid::Uuid::new_v4().to_string();
    let mut engine_job = EngineJobReporter::start_backup(
        operation.context.api.clone(),
        op_id.clone(),
        trigger,
        operation.account_scope().to_string(),
    );
    let mut terminal_progress = TerminalProgressGuard::backup(app, &op_id);
    emit_progress(
        app,
        &op_id,
        "connecting",
        0.08,
        "Connecting to database repository…",
    );
    emit_database_progress(
        app,
        profile_id,
        "connecting",
        0.08,
        "Connecting to repository…",
    );

    let result: Result<(KopiaSnapshot, u64)> = async {
        operation.ensure_not_cancelled()?;
        engine_job.progress("repository_connect");
        let (session, password) = ensure_repo_with_context(
            app,
            &operation.context,
            "backup",
            None,
            Some(&operation.control),
        )
        .await?;

        emit_progress(
            app,
            &op_id,
            "exporting",
            0.22,
            "Exporting database securely…",
        );
        emit_database_progress(app, profile_id, "exporting", 0.22, "Exporting database…");
        engine_job.progress("database_export");
        let app_c = app.clone();
        let session_c = session.clone();
        let control = Arc::clone(&operation.control);
        let kopia_args = vec![
            "snapshot".to_string(),
            "create".to_string(),
            format!("--stdin-file={stdin_filename}"),
            "--tags=backup-kind:database".to_string(),
            format!("--tags=database-profile:{profile_id}"),
            format!("--tags=savestate-profile:{profile_id}"),
            format!("--tags=savestate-trigger:{trigger}"),
            format!("--description=SaveState database backup: {profile_name}"),
            "--json".to_string(),
            "--no-progress".to_string(),
            "-".to_string(),
        ];
        let pipeline = tokio::task::spawn_blocking(move || {
            run_stream_pipeline(&app_c, source, &kopia_args, &password, &session_c, &control)
        })
        .await
        .context("Database snapshot task stopped unexpectedly")??;

        let mut snapshot = if pipeline.kopia_output.status.success() {
            serde_json::from_str::<serde_json::Value>(
                String::from_utf8_lossy(&pipeline.kopia_output.stdout).trim(),
            )
            .ok()
            .as_ref()
            .map(parse_snapshot)
        } else {
            None
        };

        // Kopia can close the pipe first (for example on a quota or repository
        // error), which then makes the dump process fail with a broken pipe.
        // Preserve the actual storage failure instead of misreporting it as a
        // database export problem.
        ensure_success(&pipeline.kopia_output, "database snapshot create")?;
        if !pipeline.source_status.success() {
            if let Some(created) = snapshot.as_ref().filter(|created| !created.id.is_empty()) {
                rollback_uncommitted_snapshot(app, operation, &session, &created.id, false).await?;
            }
            let detail = String::from_utf8_lossy(&pipeline.source_stderr);
            let detail = detail
                .lines()
                .last()
                .unwrap_or("Database export failed")
                .trim();
            return Err(anyhow!("DATABASE_EXPORT_FAILED: {detail}"));
        }
        let snapshot = snapshot
            .as_mut()
            .filter(|created| !created.id.is_empty())
            .ok_or_else(|| anyhow!("Kopia returned incomplete database snapshot metadata"))?;
        snapshot.folder = folder;
        snapshot.backup_kind = "database".to_string();
        snapshot.database_profile_id = Some(profile_id.to_string());
        snapshot.database_profile_name = Some(profile_name.to_string());
        snapshot.profile_id = Some(profile_id.to_string());
        snapshot.profile_name = Some(profile_name.to_string());
        snapshot.trigger = Some(trigger.to_string());
        snapshot.size = pipeline.source_bytes;
        snapshot.file_count = 1;

        if let Err(cancelled) = operation.ensure_not_cancelled() {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, false).await?;
            return Err(cancelled);
        }

        emit_progress(
            app,
            &op_id,
            "finalizing",
            0.82,
            "Verifying encrypted snapshot…",
        );
        emit_database_progress(
            app,
            profile_id,
            "finalizing",
            0.82,
            "Verifying encrypted snapshot…",
        );
        engine_job.progress("manifest_sync");
        if let Err(error) = upsert_manifest_snapshot(
            &operation.context.api,
            snapshot.clone(),
            keep_latest.map(|keep| (profile_id, keep)),
        )
        .await
        {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, true).await?;
            return Err(error.context(
                "Database snapshot was rolled back because metadata could not be synchronized",
            ));
        }
        if let Err(cancelled) = operation.control.mark_committed().await {
            rollback_uncommitted_snapshot(app, operation, &session, &snapshot.id, true).await?;
            return Err(cancelled);
        }
        Ok((snapshot.clone(), pipeline.source_bytes))
    }
    .await;

    match result {
        Ok((snapshot, source_bytes)) => {
            emit_progress(app, &op_id, "done", 1.0, "Database backup complete");
            emit_database_progress(app, profile_id, "done", 1.0, "Database backup complete");
            terminal_progress.finish();
            engine_job.finish("succeeded", "completed", Some(source_bytes), Some(1), None);
            if operation
                .api()
                .enforce_retention()
                .await
                .map(|result| result.maintenance_recommended)
                .unwrap_or(false)
            {
                schedule_storage_cleanup(app.clone());
            }
            Ok(snapshot.id)
        }
        Err(error) if backup_operations::is_cancelled(&error) => {
            emit_progress(app, &op_id, "cancelled", 0.0, "Database backup stopped");
            emit_database_progress(app, profile_id, "cancelled", 0.0, "Database backup stopped");
            terminal_progress.finish();
            engine_job.finish("cancelled", "cancelled", None, None, Some("user_logout"));
            Err(error)
        }
        Err(error) => {
            emit_progress(app, &op_id, "error", 0.0, &format!("{error:#}"));
            emit_database_progress(
                app,
                profile_id,
                "error",
                0.0,
                "Database backup failed. Check the error message and try again.",
            );
            engine_job.fail_with_error("database_backup_failed", &error, None, None);
            terminal_progress.finish();
            Err(error)
        }
    }
}

/// Create a deduplicated, compressed snapshot of `source_path`.
pub async fn backup_path(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    source_path: &str,
) -> Result<String> {
    backup_paths(app, state, vec![source_path.to_string()]).await
}

pub async fn backup_path_with_trigger(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    source_path: &str,
    trigger: &'static str,
    folder: &str,
) -> Result<String> {
    backup_paths_with_trigger(app, state, vec![source_path.to_string()], trigger, folder).await
}

fn normalize_snapshot_folder(folder: &str) -> Result<String> {
    let replaced = folder.trim().replace('\\', "/");
    if replaced.is_empty() || replaced == "/" {
        return Ok(root_folder());
    }
    let segments: Vec<&str> = replaced
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() || segments.len() > 8 {
        return Err(anyhow!("Invalid backup folder path"));
    }
    for segment in &segments {
        if segment.len() > 50
            || *segment == "."
            || *segment == ".."
            || !segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, ' ' | '_' | '-')
            })
        {
            return Err(anyhow!("Invalid backup folder path"));
        }
    }
    Ok(format!("/{}", segments.join("/")))
}

/// Delete a snapshot from the repository.
pub(crate) async fn delete_snapshot_with_lease(
    app: &tauri::AppHandle,
    engine: &EngineLease<'_>,
    state: &AppStateWrapper,
    snapshot_id: &str,
) -> Result<()> {
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    delete_snapshot_with_context(app, engine, &context, snapshot_id).await
}

async fn delete_snapshot_with_context(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    context: &AccountContext,
    snapshot_id: &str,
) -> Result<()> {
    context.ensure_current(app.state::<AppStateWrapper>().inner())?;
    let api = context.api.clone();
    let mut engine_job = EngineJobReporter::start(
        api.clone(),
        uuid::Uuid::new_v4().to_string(),
        "delete",
        "manual",
    );
    engine_job.progress("repository_connect");
    let (session, password) = ensure_repo_with_context(app, &context, "backup", None, None).await?;
    let app_c = app.clone();
    let session_c = session.clone();

    let snapshot_id_c = snapshot_id.to_string();
    engine_job.progress("snapshot_delete");
    tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec![
            "snapshot".to_string(),
            "delete".to_string(),
            snapshot_id_c,
            "--delete".to_string(),
        ];
        let out = run_kopia(&app_c, &args, Some(&password), Some(&session_c))?;
        if out.status.success() {
            return Ok(());
        }

        // Deletion is idempotent. If a previous attempt removed the repository
        // snapshot but failed while updating the server manifest, retrying must
        // still reconcile the manifest instead of getting stuck forever.
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
        .to_ascii_lowercase();
        if output.contains("no snapshots matched") {
            return Ok(());
        }

        ensure_success(&out, "snapshot delete")
    })
    .await
    .context("kopia delete task panicked")??;

    engine_job.progress("manifest_sync");
    remove_manifest_snapshot(&api, snapshot_id)
        .await
        .context("Snapshot was deleted, but its server manifest could not be synchronized")?;
    engine_job.finish("succeeded", "completed", None, None, None);
    Ok(())
}

/// List all snapshots in the repository.
pub async fn list_snapshots(
    _app: &tauri::AppHandle,
    state: &AppStateWrapper,
) -> Result<Vec<KopiaSnapshot>> {
    // Listing is served from the authenticated manifest. This deliberately
    // avoids issuing read credentials before restore egress is authorized.
    let manifest = api_from_state(state)?.get_kopia_manifest().await?;
    serde_json::from_value(manifest).context("Invalid kopia snapshot manifest")
}

/// Delete only excess versions that still live in the managed profile folder.
/// A version deliberately moved elsewhere is a user-pinned restore point and
/// is excluded from both automatic retention and profile-folder deletion.
pub async fn prune_profile_snapshots(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    profile_id: &str,
    profile_folder: &str,
    keep_latest: usize,
) -> Result<Vec<String>> {
    let engine = begin_operation().await?;
    if keep_latest == 0 {
        return Ok(Vec::new());
    }
    let expired = expired_profile_snapshot_ids(
        list_snapshots(app, state).await?,
        profile_id,
        profile_folder,
        keep_latest,
    );
    for snapshot_id in &expired {
        delete_snapshot_with_lease(app, &engine, state, snapshot_id).await?;
    }
    if !expired.is_empty() {
        schedule_storage_cleanup(app.clone());
    }
    Ok(expired)
}

pub async fn prune_profile_snapshots_with_operation(
    app: &tauri::AppHandle,
    engine: &EngineLease<'_>,
    operation: &BackupOperation,
    profile_id: &str,
    profile_folder: &str,
    keep_latest: usize,
) -> Result<Vec<String>> {
    if keep_latest == 0 {
        return Ok(Vec::new());
    }
    let manifest = operation.api().get_kopia_manifest().await?;
    let snapshots: Vec<KopiaSnapshot> =
        serde_json::from_value(manifest).context("Invalid kopia snapshot manifest")?;
    let expired = expired_profile_snapshot_ids(snapshots, profile_id, profile_folder, keep_latest);
    for snapshot_id in &expired {
        delete_snapshot_with_context(app, engine, &operation.context, snapshot_id).await?;
    }
    if !expired.is_empty() {
        schedule_storage_cleanup_with_context(app.clone(), operation.context.clone());
    }
    Ok(expired)
}

fn expired_profile_snapshot_ids(
    snapshots: Vec<KopiaSnapshot>,
    profile_id: &str,
    profile_folder: &str,
    keep_latest: usize,
) -> Vec<String> {
    if keep_latest == 0 {
        return Vec::new();
    }
    let mut snapshots: Vec<KopiaSnapshot> = snapshots
        .into_iter()
        .filter(|snapshot| {
            snapshot.profile_id.as_deref() == Some(profile_id)
                && crate::backup::folder_contains(profile_folder, &snapshot.folder)
        })
        .collect();
    snapshots.sort_by(|left, right| {
        right
            .version_number
            .cmp(&left.version_number)
            .then_with(|| right.start_time.cmp(&left.start_time))
    });
    snapshots
        .into_iter()
        .skip(keep_latest)
        .map(|snapshot| snapshot.id)
        .collect()
}

async fn list_snapshots_from_repository(
    app: &tauri::AppHandle,
    context: &AccountContext,
) -> Result<Vec<KopiaSnapshot>> {
    let (session, password) = ensure_repo_with_context(app, context, "backup", None, None).await?;
    let app_c = app.clone();
    let session_c = session.clone();
    tokio::task::spawn_blocking(move || -> Result<Vec<KopiaSnapshot>> {
        let args = vec![
            "snapshot".to_string(),
            "list".to_string(),
            "--all".to_string(),
            "--json".to_string(),
        ];
        let out = run_kopia(&app_c, &args, Some(&password), Some(&session_c))?;
        ensure_success(&out, "snapshot list")?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: serde_json::Value =
            serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Array(vec![]));

        let mut result = Vec::new();
        if let Some(arr) = parsed.as_array() {
            for item in arr {
                result.push(parse_snapshot(item));
            }
        }
        Ok(result)
    })
    .await
    .context("kopia list task panicked")?
}

fn parse_snapshot(item: &serde_json::Value) -> KopiaSnapshot {
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source_path = item
        .get("source")
        .and_then(|s| s.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let start_time = item
        .get("startTime")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let summ = item.get("rootEntry").and_then(|r| r.get("summ"));
    let size = summ
        .and_then(|s| s.get("size"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let file_count = summ
        .and_then(|s| s.get("files"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tags = item.get("tags").and_then(|value| value.as_object());
    let backup_kind = tags
        .and_then(|tags| tags.get("tag:backup-kind"))
        .and_then(|value| value.as_str())
        .unwrap_or("files")
        .to_string();
    let database_profile_id = tags
        .and_then(|tags| tags.get("tag:database-profile"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string);
    let profile_id = tags
        .and_then(|tags| tags.get("tag:savestate-profile"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| database_profile_id.clone());
    let trigger = tags
        .and_then(|tags| tags.get("tag:savestate-trigger"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string);
    let root_object_id = item
        .get("rootEntry")
        .and_then(|root| root.get("obj"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string);

    KopiaSnapshot {
        id,
        source_path,
        start_time,
        size,
        file_count,
        folder: root_folder(),
        backup_kind,
        database_profile_id,
        database_profile_name: None,
        root_object_id,
        profile_id,
        profile_name: None,
        trigger,
        version_number: None,
    }
}

async fn upload_manifest_with_retry(
    api: &SaveStateClient,
    snapshots: &[KopiaSnapshot],
    retention: Option<(&str, usize)>,
) -> Result<()> {
    let json = serde_json::to_string(snapshots)?;
    let mut last_error = None;
    for attempt in 1..=3 {
        match api.upload_kopia_manifest(&json, retention).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let message = error.to_string();
                // A source-data allowance rejection is deterministic. Retrying
                // only delays rollback and leaves the progress UI running.
                if message.contains("SOURCE_QUOTA_EXCEEDED")
                    || message.contains("507 Insufficient Storage")
                {
                    return Err(error);
                }
                last_error = Some(message);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
                }
            }
        }
    }

    Err(anyhow!(
        "Failed to upload Kopia manifest after 3 attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

async fn upsert_manifest_snapshot(
    api: &SaveStateClient,
    snapshot: KopiaSnapshot,
    retention: Option<(&str, usize)>,
) -> Result<()> {
    let _manifest_guard = manifest_update_lock().lock().await;
    let current = api.get_kopia_manifest().await?;
    let mut snapshots: Vec<KopiaSnapshot> =
        serde_json::from_value(current).context("Invalid Kopia snapshot manifest")?;
    snapshots.retain(|item| item.id != snapshot.id);
    snapshots.push(snapshot);
    snapshots.sort_by(|a, b| b.start_time.cmp(&a.start_time));
    upload_manifest_with_retry(api, &snapshots, retention).await
}

async fn remove_manifest_snapshot(api: &SaveStateClient, snapshot_id: &str) -> Result<()> {
    let _manifest_guard = manifest_update_lock().lock().await;
    let current = api.get_kopia_manifest().await?;
    let mut snapshots: Vec<KopiaSnapshot> =
        serde_json::from_value(current).context("Invalid Kopia snapshot manifest")?;
    snapshots.retain(|item| item.id != snapshot_id);
    upload_manifest_with_retry(api, &snapshots, None).await
}

fn database_content_object(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
    root_object_id: &str,
) -> Result<String> {
    let args = vec![
        "content".to_string(),
        "show".to_string(),
        root_object_id.to_string(),
        "--json".to_string(),
    ];
    let output = run_kopia(app, &args, Some(password), Some(session))?;
    ensure_success(&output, "database snapshot lookup")?;
    let directory: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Database snapshot directory metadata is invalid")?;
    database_content_object_from_value(&directory)
}

pub(crate) fn database_content_object_from_value(directory: &serde_json::Value) -> Result<String> {
    directory
        .get("entries")
        .and_then(|entries| entries.as_array())
        .and_then(|entries| {
            entries.iter().find(|entry| {
                entry.get("type").and_then(|value| value.as_str()) == Some("f")
                    && entry
                        .get("name")
                        .and_then(|value| value.as_str())
                        .is_some_and(|name| name.ends_with("database.sql"))
            })
        })
        .and_then(|entry| entry.get("obj"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("Database SQL stream is missing from this snapshot"))
}

fn run_database_restore_pipeline(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
    content_object_id: &str,
    target: StreamRestoreCommand,
    snapshot_id: &str,
) -> Result<u64> {
    let content_args = vec![
        "content".to_string(),
        "show".to_string(),
        content_object_id.to_string(),
    ];
    let kopia_command = build_kopia_command(app, &content_args, Some(password), Some(session));
    run_database_restore_pipeline_commands(kopia_command, target, snapshot_id)
}

pub(crate) fn run_database_restore_pipeline_commands(
    mut kopia_command: Command,
    target: StreamRestoreCommand,
    snapshot_id: &str,
) -> Result<u64> {
    kopia_command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut kopia_child = kopia_command
        .spawn()
        .context("Could not start the encrypted database restore stream")?;

    let mut target_command = Command::new(&target.program);
    target_command
        .args(&target.args)
        .envs(target.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        target_command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut target_child = match target_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = kopia_child.kill();
            let _ = kopia_child.wait();
            return Err(error).with_context(|| {
                format!(
                    "Could not start database restore tool at {}",
                    target.program.display()
                )
            });
        }
    };

    let mut kopia_stdout = kopia_child
        .stdout
        .take()
        .context("Could not read decrypted database snapshot")?;
    let mut kopia_stderr = kopia_child
        .stderr
        .take()
        .context("Could not capture database snapshot errors")?;
    let mut target_stdin = target_child
        .stdin
        .take()
        .context("Could not open database restore input")?;
    let mut target_stdout = target_child
        .stdout
        .take()
        .context("Could not capture database restore output")?;
    let mut target_stderr = target_child
        .stderr
        .take()
        .context("Could not capture database restore errors")?;

    let transferred = Arc::new(AtomicU64::new(0));
    let transferred_writer = Arc::clone(&transferred);
    let stream_thread = std::thread::spawn(move || -> std::io::Result<()> {
        let copied = std::io::copy(&mut kopia_stdout, &mut target_stdin)?;
        transferred_writer.store(copied, Ordering::Release);
        Ok(())
    });
    let kopia_error_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = kopia_stderr.read_to_end(&mut output);
        output
    });
    let target_output_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = target_stdout.read_to_end(&mut output);
        output
    });
    let target_error_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = target_stderr.read_to_end(&mut output);
        output
    });

    let mut kopia_status = None;
    let mut target_status = None;
    loop {
        if restore_is_cancelled(snapshot_id) {
            let _ = kopia_child.kill();
            let _ = target_child.kill();
            let _ = kopia_child.wait();
            let _ = target_child.wait();
            let _ = stream_thread.join();
            let _ = kopia_error_thread.join();
            let _ = target_output_thread.join();
            let _ = target_error_thread.join();
            return Err(anyhow!("Restore cancelled"));
        }
        if kopia_status.is_none() {
            kopia_status = kopia_child
                .try_wait()
                .context("Failed to poll database snapshot stream")?;
        }
        if target_status.is_none() {
            target_status = target_child
                .try_wait()
                .context("Failed to poll database restore")?;
        }
        if kopia_status.is_some_and(|status| !status.success()) && target_status.is_none() {
            let _ = target_child.kill();
        }
        if target_status.is_some_and(|status| !status.success()) && kopia_status.is_none() {
            let _ = kopia_child.kill();
        }
        if kopia_status.is_some() && target_status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let stream_result = stream_thread
        .join()
        .map_err(|_| anyhow!("Database restore stream worker stopped unexpectedly"))?;
    let kopia_stderr = kopia_error_thread
        .join()
        .map_err(|_| anyhow!("Could not collect database snapshot errors"))?;
    let _target_stdout = target_output_thread
        .join()
        .map_err(|_| anyhow!("Could not collect database restore output"))?;
    let target_stderr = target_error_thread
        .join()
        .map_err(|_| anyhow!("Could not collect database restore errors"))?;
    let kopia_status = kopia_status.context("Database snapshot stream did not exit")?;
    let target_status = target_status.context("Database restore process did not exit")?;
    let target_detail = String::from_utf8_lossy(&target_stderr);
    let target_detail = target_detail.trim();
    if !target_status.success() && !target_detail.is_empty() {
        let detail = target_detail
            .lines()
            .last()
            .unwrap_or("The database rejected the SQL restore")
            .trim();
        return Err(anyhow!("DATABASE_RESTORE_FAILED: {detail}"));
    }
    if !kopia_status.success() {
        return Err(classify_kopia_error(
            "database snapshot stream",
            &String::from_utf8_lossy(&kopia_stderr),
        ));
    }
    if !target_status.success() {
        return Err(anyhow!(
            "DATABASE_RESTORE_FAILED: The database restore tool exited before accepting the SQL stream"
        ));
    }
    stream_result.context("The database restore stream was interrupted")?;
    Ok(transferred.load(Ordering::Acquire))
}

/// Decrypt and stream one database snapshot directly into a database client.
/// No plaintext SQL file is created during restore.
pub async fn restore_database_snapshot_to_command(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    state: &AppStateWrapper,
    snapshot_id: &str,
    profile_id: &str,
    target: StreamRestoreCommand,
) -> Result<()> {
    let op_id = uuid::Uuid::new_v4().to_string();
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
        AccountContext::capture(&guard)?
    };
    let api = context.api.clone();
    let mut engine_job =
        EngineJobReporter::start(api.clone(), op_id.clone(), "restore", "database_manual");
    let mut terminal_progress = TerminalProgressGuard::restore(app, &op_id);
    clear_restore_cancellation(snapshot_id);
    emit_restore_progress(
        app,
        &op_id,
        "preparing",
        0.08,
        "Preparing database restore…",
    );
    emit_database_restore_progress(app, profile_id, "preparing", 0.08, "Preparing restore…");

    engine_job.progress("manifest_lookup");
    let manifest = api.get_kopia_manifest().await?;
    let snapshots: Vec<KopiaSnapshot> =
        serde_json::from_value(manifest).context("Invalid Kopia snapshot manifest")?;
    let snapshot = snapshots
        .into_iter()
        .find(|snapshot| snapshot.id == snapshot_id)
        .ok_or_else(|| anyhow!("Database snapshot not found"))?;
    if snapshot.backup_kind != "database"
        || snapshot.database_profile_id.as_deref() != Some(profile_id)
    {
        return Err(anyhow!(
            "This restore point does not belong to the selected database profile"
        ));
    }
    let root_object_id = snapshot
        .root_object_id
        .clone()
        .ok_or_else(|| anyhow!("This database restore point is missing stream metadata"))?;

    engine_job.progress("authorization");
    let authorization = api.restore_authorize(snapshot_id, snapshot.size).await?;
    if !authorization.authorized {
        return Err(anyhow!("Database restore was not authorized"));
    }
    ensure_restore_not_cancelled(snapshot_id)?;

    emit_restore_progress(
        app,
        &op_id,
        "restoring",
        0.3,
        "Downloading and decrypting database…",
    );
    emit_database_restore_progress(
        app,
        profile_id,
        "restoring",
        0.3,
        "Downloading and importing…",
    );
    engine_job.progress("repository_connect");
    let (session, password) = ensure_repo_with_context(
        app,
        &context,
        "restore",
        authorization.grant_id.as_deref(),
        None,
    )
    .await?;
    ensure_restore_not_cancelled(snapshot_id)?;

    let app_c = app.clone();
    let session_c = session.clone();
    let snapshot_id_owned = snapshot_id.to_string();
    engine_job.progress("database_import");
    let transferred = tokio::task::spawn_blocking(move || -> Result<u64> {
        let content_object =
            database_content_object(&app_c, &session_c, &password, &root_object_id)?;
        run_database_restore_pipeline(
            &app_c,
            &session_c,
            &password,
            &content_object,
            target,
            &snapshot_id_owned,
        )
    })
    .await
    .context("Database restore task stopped unexpectedly")?;

    match transferred {
        Ok(bytes) => {
            clear_restore_cancellation(snapshot_id);
            emit_restore_progress(app, &op_id, "done", 1.0, "Database restore complete");
            emit_database_restore_progress(
                app,
                profile_id,
                "done",
                1.0,
                "Database restore complete",
            );
            terminal_progress.finish();
            engine_job.finish("succeeded", "completed", Some(bytes), Some(1), None);
            Ok(())
        }
        Err(error) => {
            let was_cancelled = restore_is_cancelled(snapshot_id)
                || error.to_string().to_ascii_lowercase().contains("cancelled");
            clear_restore_cancellation(snapshot_id);
            emit_restore_progress(
                app,
                &op_id,
                if was_cancelled { "cancelled" } else { "error" },
                0.0,
                if was_cancelled {
                    "Database restore cancelled"
                } else {
                    "Database restore failed"
                },
            );
            emit_database_restore_progress(
                app,
                profile_id,
                if was_cancelled { "cancelled" } else { "error" },
                0.0,
                if was_cancelled {
                    "Database restore stopped"
                } else {
                    "Database restore failed"
                },
            );
            terminal_progress.finish();
            if was_cancelled {
                engine_job.finish("cancelled", "cancelled", Some(snapshot.size), Some(1), None);
            } else {
                engine_job.fail("database_restore_failed", Some(snapshot.size), Some(1));
            }
            Err(error)
        }
    }
}

/// Restore a snapshot to `target_path`. Requests a short-lived authorization
/// grant before issuing repository read credentials; restore traffic is free.
pub async fn restore_snapshot(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    snapshot_id: &str,
    target_path: &str,
) -> Result<()> {
    let engine = begin_operation().await?;
    restore_snapshot_with_lease(app, &engine, state, snapshot_id, target_path).await
}

pub(crate) async fn restore_snapshot_with_lease(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    state: &AppStateWrapper,
    snapshot_id: &str,
    target_path: &str,
) -> Result<()> {
    let op_id = uuid::Uuid::new_v4().to_string();
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    let api = context.api.clone();
    let mut engine_job = EngineJobReporter::start(api.clone(), op_id.clone(), "restore", "manual");
    let mut terminal_progress = TerminalProgressGuard::restore(app, &op_id);
    clear_restore_cancellation(snapshot_id);
    emit_restore_progress(app, &op_id, "preparing", 0.1, "Preparing restore…");

    // Determine the snapshot size for replay-safe operational transfer telemetry.
    engine_job.progress("manifest_lookup");
    let manifest = api.get_kopia_manifest().await?;
    let snapshots: Vec<KopiaSnapshot> =
        serde_json::from_value(manifest).context("Invalid kopia snapshot manifest")?;
    let snap = snapshots
        .iter()
        .find(|s| s.id == snapshot_id)
        .ok_or_else(|| anyhow!("Snapshot not found: {}", snapshot_id))?;
    if snap.size == 0 {
        return Err(anyhow!(
            "Snapshot size is unavailable; refresh the backup list and try again"
        ));
    }

    // Request the short-lived restore grant. A 403 now means authorization or
    // service access failed, never that a paid transfer allowance was exceeded.
    engine_job.progress("authorization");
    let authorization = api.restore_authorize(snapshot_id, snap.size).await?;
    if !authorization.authorized {
        return Err(anyhow!("Restore was not authorized"));
    }
    if let Err(error) = ensure_restore_not_cancelled(snapshot_id) {
        emit_restore_progress(app, &op_id, "cancelled", 0.0, "Restore cancelled");
        terminal_progress.finish();
        clear_restore_cancellation(snapshot_id);
        engine_job.finish(
            "cancelled",
            "cancelled",
            Some(snap.size),
            Some(snap.file_count),
            None,
        );
        return Err(error);
    }

    emit_restore_progress(app, &op_id, "restoring", 0.4, "Downloading and decrypting…");

    // Older production APIs authorize and meter the declared byte count but do
    // not yet return a grant. Newer APIs return a one-time grant and require it
    // for the read-only repository session. Supporting both lets the client be
    // rolled out safely before the stricter API deployment.
    engine_job.progress("repository_connect");
    let (session, password) = ensure_repo_with_context(
        app,
        &context,
        "restore",
        authorization.grant_id.as_deref(),
        None,
    )
    .await?;
    if let Err(error) = ensure_restore_not_cancelled(snapshot_id) {
        emit_restore_progress(app, &op_id, "cancelled", 0.0, "Restore cancelled");
        terminal_progress.finish();
        clear_restore_cancellation(snapshot_id);
        engine_job.finish(
            "cancelled",
            "cancelled",
            Some(snap.size),
            Some(snap.file_count),
            None,
        );
        return Err(error);
    }
    let app_c = app.clone();
    let session_c = session.clone();
    let snapshot = snapshot_id.to_string();
    let cancellation_id = snapshot_id.to_string();
    let target = target_path.to_string();

    engine_job.progress("snapshot_restore");
    let restore_result = tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec![
            "restore".to_string(),
            snapshot,
            target,
            "--no-progress".to_string(),
        ];
        let out = run_kopia_cancellable(
            &app_c,
            &args,
            Some(&password),
            Some(&session_c),
            &cancellation_id,
        )?;
        ensure_success(&out, "restore")
    })
    .await
    .context("kopia restore task panicked")?;

    if let Err(error) = restore_result {
        let was_cancelled = restore_is_cancelled(snapshot_id)
            || error.to_string().to_ascii_lowercase().contains("cancelled");
        emit_restore_progress(
            app,
            &op_id,
            if was_cancelled { "cancelled" } else { "error" },
            0.0,
            if was_cancelled {
                "Restore cancelled"
            } else {
                "Restore failed"
            },
        );
        terminal_progress.finish();
        clear_restore_cancellation(snapshot_id);
        if was_cancelled {
            engine_job.finish(
                "cancelled",
                "cancelled",
                Some(snap.size),
                Some(snap.file_count),
                None,
            );
        } else {
            engine_job.fail("restore_failed", Some(snap.size), Some(snap.file_count));
        }
        return Err(error);
    }

    clear_restore_cancellation(snapshot_id);
    emit_restore_progress(app, &op_id, "done", 1.0, "Restore complete");
    terminal_progress.finish();
    engine_job.finish(
        "succeeded",
        "completed",
        Some(snap.size),
        Some(snap.file_count),
        None,
    );
    Ok(())
}

/// Apply a FIFO retention policy: keep only the latest `keep_latest` snapshots.
/// Kopia prunes older snapshots and dedup blocks are reclaimed by maintenance.
pub async fn set_retention(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    keep_latest: u32,
) -> Result<()> {
    let engine = begin_operation().await?;
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    set_retention_with_context(app, &engine, &context, keep_latest, None).await
}

pub async fn set_retention_with_operation(
    app: &tauri::AppHandle,
    engine: &EngineLease<'_>,
    operation: &BackupOperation,
    keep_latest: u32,
) -> Result<()> {
    set_retention_with_context(
        app,
        engine,
        &operation.context,
        keep_latest,
        Some(&operation.control),
    )
    .await
}

async fn set_retention_with_context(
    app: &tauri::AppHandle,
    _engine: &EngineLease<'_>,
    context: &AccountContext,
    keep_latest: u32,
    cancellation: Option<&Arc<BackupControl>>,
) -> Result<()> {
    context.ensure_current(app.state::<AppStateWrapper>().inner())?;
    if last_retention()
        .lock()
        .map(|retention| {
            retention
                .as_ref()
                .map(|cached| {
                    cached.account_scope == context.account_scope
                        && cached.session_generation == context.session_generation
                        && cached.keep_latest == keep_latest
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return Ok(());
    }

    let (session, password) =
        ensure_repo_with_context(app, context, "backup", None, cancellation).await?;
    let app_c = app.clone();
    let session_c = session.clone();
    let cancellation = cancellation.cloned();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec![
            "policy".to_string(),
            "set".to_string(),
            "--global".to_string(),
            format!("--keep-latest={}", keep_latest),
            "--keep-hourly=0".to_string(),
            "--keep-daily=0".to_string(),
            "--keep-weekly=0".to_string(),
            "--keep-monthly=0".to_string(),
            "--keep-annual=0".to_string(),
        ];
        let out = run_kopia_for_backup(
            &app_c,
            &args,
            Some(&password),
            Some(&session_c),
            cancellation.as_ref(),
        )?;
        ensure_success(&out, "policy set")
    })
    .await
    .context("kopia policy task panicked")??;

    if let Ok(mut retention) = last_retention().lock() {
        *retention = Some(CachedRetention {
            account_scope: context.account_scope.clone(),
            session_generation: context.session_generation,
            keep_latest,
        });
    }

    Ok(())
}

fn maintenance_owner_from_config(session: &RepoSession) -> Result<String> {
    let data = std::fs::read_to_string(kopia_config_file(session))
        .context("Failed to read Kopia repository config")?;
    let config: serde_json::Value =
        serde_json::from_str(&data).context("Invalid Kopia repository config")?;
    let username = config
        .get("username")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .context("Kopia repository username is missing")?;
    let hostname = config
        .get("hostname")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .context("Kopia repository hostname is missing")?;
    Ok(format!("{}@{}", username, hostname))
}

/// Run repository maintenance (reclaims space from pruned snapshots).
/// Maintenance takes the engine gate exclusively so Kopia never performs a
/// backup, restore, deletion, or application update at the same time.
async fn run_maintenance_with_context(
    app: &tauri::AppHandle,
    context: &AccountContext,
) -> Result<()> {
    let _operation_guard = operation_gate().write().await;
    // Queued maintenance can outlive its account/workspace. Revalidate only
    // after obtaining the lease, before any repository request or mutation.
    context.ensure_current(app.state::<AppStateWrapper>().inner())?;
    let mut engine_job = EngineJobReporter::start(
        context.api.clone(),
        uuid::Uuid::new_v4().to_string(),
        "maintenance",
        "automatic",
    );
    engine_job.progress("repository_connect");
    let (session, password) = ensure_repo_with_context(app, &context, "backup", None, None).await?;
    let app_c = app.clone();
    let session_c = session.clone();

    engine_job.progress("maintenance_run");
    tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec![
            "maintenance".to_string(),
            "run".to_string(),
            "--full".to_string(),
            "--no-progress".to_string(),
        ];
        let first = run_kopia(&app_c, &args, Some(&password), Some(&session_c))?;
        if first.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&first.stderr).to_ascii_lowercase();
        if !stderr.contains("designated user") {
            return ensure_success(&first, "maintenance run");
        }

        // A repository created on an older PC keeps that machine as its full
        // maintenance owner. Transfer ownership to the current connected
        // Kopia identity once, then retry.
        let owner = maintenance_owner_from_config(&session_c)?;
        let set_owner = vec![
            "maintenance".to_string(),
            "set".to_string(),
            format!("--owner={}", owner),
            "--no-progress".to_string(),
        ];
        let changed = run_kopia(&app_c, &set_owner, Some(&password), Some(&session_c))?;
        ensure_success(&changed, "maintenance owner update")?;

        let retry = run_kopia(&app_c, &args, Some(&password), Some(&session_c))?;
        ensure_success(&retry, "maintenance run")
    })
    .await
    .context("kopia maintenance task panicked")??;

    engine_job.finish("succeeded", "completed", None, None, None);
    Ok(())
}

pub async fn run_maintenance(app: &tauri::AppHandle, state: &AppStateWrapper) -> Result<()> {
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    run_maintenance_with_context(app, &context).await
}

/// Reclaim obsolete packs before a near-capacity backup. The commercial plan
/// is measured from physical repository usage, but the gateway retains a small
/// private safety buffer so a failed maintenance attempt does not destroy an
/// otherwise valid backup. The gateway remains the final hard safety ceiling.
pub async fn prepare_repository_for_backup(
    app: &tauri::AppHandle,
    operation: &BackupOperation,
) -> Result<()> {
    operation.ensure_not_cancelled()?;
    let pressure = match operation.api().enforce_retention().await {
        Ok(pressure) => pressure,
        Err(error) => {
            eprintln!(
                "Could not check repository pressure before backup: {}",
                error
            );
            return Ok(());
        }
    };
    if !pressure.maintenance_recommended
        || (cleanup_within_cooldown(operation.account_scope()) && !pressure.maintenance_urgent)
    {
        return Ok(());
    }
    if CLEANUP_RUNNING.swap(true, Ordering::AcqRel) {
        if pressure.maintenance_urgent {
            // A dashboard/deletion cleanup may have been queued just before
            // this backup. Wait for it instead of letting the backup race the
            // 750 ms scheduling delay and spend reclamation headroom first.
            for _ in 0..1_800 {
                operation.ensure_not_cancelled()?;
                if !CLEANUP_RUNNING.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if CLEANUP_RUNNING.load(Ordering::Acquire) {
                return Err(anyhow!(
                    "OPTIMIZED_STORAGE_QUOTA_EXCEEDED: Storage cleanup is still running. Retry this backup when cleanup finishes."
                ));
            }
            if operation
                .api()
                .enforce_retention()
                .await
                .map(|current| current.maintenance_urgent)
                .unwrap_or(false)
            {
                return Err(anyhow!(
                    "OPTIMIZED_STORAGE_QUOTA_EXCEEDED: Your optimized backup storage is full after cleanup. Remove a retained backup or choose a larger plan."
                ));
            }
        }
        return Ok(());
    }

    emit_storage_cleanup(
        app,
        "running",
        if pressure.maintenance_urgent {
            "Storage is full. Reclaiming expired backup data before retrying…"
        } else {
            "Optimizing repository storage before backup…"
        },
    );
    let result = run_maintenance_with_context(app, &operation.context).await;
    CLEANUP_RUNNING.store(false, Ordering::Release);
    match result {
        Ok(()) => {
            mark_cleanup_complete(operation.account_scope());
            emit_storage_cleanup(app, "complete", "Repository optimization completed");
        }
        Err(error) => {
            emit_storage_cleanup(
                app,
                "failed",
                &format!("Repository optimization could not finish: {}", error),
            );
            eprintln!("Pre-backup repository maintenance failed: {}", error);
        }
    }
    operation.ensure_not_cancelled()?;
    if pressure.maintenance_urgent
        && operation
            .api()
            .enforce_retention()
            .await
            .map(|current| current.maintenance_urgent)
            .unwrap_or(false)
    {
        return Err(anyhow!(
            "OPTIMIZED_STORAGE_QUOTA_EXCEEDED: Your optimized backup storage is full after cleanup. Remove a retained backup or choose a larger plan."
        ));
    }
    Ok(())
}

/// Queue full maintenance after deletion without keeping the delete button
/// blocked. Calls are coalesced and rate-limited because safe Kopia cleanup can
/// require multiple maintenance cycles before remote objects are reclaimable.
pub fn schedule_storage_cleanup(app: tauri::AppHandle) -> &'static str {
    let context = {
        let state = app.state::<AppStateWrapper>();
        let guard = match state.0.lock() {
            Ok(guard) => guard,
            Err(_) => {
                CLEANUP_RUNNING.store(false, Ordering::Release);
                return "unavailable";
            }
        };
        match AccountContext::capture(&guard) {
            Ok(context) => context,
            Err(_) => {
                CLEANUP_RUNNING.store(false, Ordering::Release);
                return "unavailable";
            }
        }
    };

    schedule_storage_cleanup_with_context(app, context)
}

fn schedule_storage_cleanup_with_context(
    app: tauri::AppHandle,
    context: AccountContext,
) -> &'static str {
    if CLEANUP_RUNNING.swap(true, Ordering::AcqRel) {
        return "running";
    }

    if cleanup_within_cooldown(&context.account_scope) {
        CLEANUP_RUNNING.store(false, Ordering::Release);
        return "cooldown";
    }

    emit_storage_cleanup(
        &app,
        "pending",
        "Deleted storage is queued for safe cleanup",
    );
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(750)).await;
        emit_storage_cleanup(&app, "running", "Reclaiming deleted storage…");

        let result = run_maintenance_with_context(&app, &context).await;

        if result.is_ok() {
            mark_cleanup_complete(&context.account_scope);
        }
        CLEANUP_RUNNING.store(false, Ordering::Release);

        match result {
            Ok(()) => emit_storage_cleanup(
                &app,
                "complete",
                "Storage cleanup completed. Usage is refreshing…",
            ),
            Err(error) => emit_storage_cleanup(
                &app,
                "failed",
                &format!("Storage cleanup could not finish: {}", error),
            ),
        }
    });

    "scheduled"
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands
// ────────────────────────────────────────────────────────────────────

/// Connect to (or initialize) the authenticated backup repository without
/// blocking the frontend. The connected short-lived session is cached so the
/// first backup can begin scanning immediately.
#[tauri::command]
pub async fn cmd_warm_repository(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
) -> Result<(), String> {
    let _operation_guard = begin_operation().await.map_err(|error| error.to_string())?;
    ensure_repo(&app, state.inner(), "backup", None)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_kopia_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    source_path: String,
) -> Result<String, String> {
    let api = api_from_state(state.inner()).map_err(|error| error.to_string())?;
    let display_name = std::path::Path::new(&source_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("Quick backup")
        .to_string();
    let result = backup_path(&app, state.inner(), &source_path).await;
    match &result {
        Ok(backup_id) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_success",
                &display_name,
                &format!("Quick backup completed. ID: {}", backup_id),
            )
            .await;
        }
        Err(error) if error.to_string().contains("BACKUP_CANCELLED") => {}
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_failure",
                &display_name,
                "Quick backup failed. Open SaveState Vault for details.",
            )
            .await;
        }
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_kopia_list_snapshots(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
) -> Result<Vec<KopiaSnapshot>, String> {
    list_snapshots(&app, state.inner())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_kopia_restore(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    snapshot_id: String,
    target_path: String,
) -> Result<(), String> {
    let api = api_from_state(state.inner()).map_err(|error| error.to_string())?;
    let result = restore_snapshot(&app, state.inner(), &snapshot_id, &target_path).await;
    match &result {
        Ok(()) => {
            crate::notifications::send_backup_notification(
                &api,
                "restore_success",
                "Snapshot restore",
                &format!("Restore completed. Snapshot: {}", snapshot_id),
            )
            .await;
        }
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "restore_failure",
                "Snapshot restore",
                "Restore failed. Open SaveState Vault for details.",
            )
            .await;
        }
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_kopia_set_retention(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    keep_latest: u32,
) -> Result<(), String> {
    set_retention(&app, state.inner(), keep_latest)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_kopia_maintenance(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
) -> Result<(), String> {
    run_maintenance(&app, state.inner())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn cmd_schedule_storage_cleanup(app: tauri::AppHandle) -> String {
    schedule_storage_cleanup(app).to_string()
}

pub async fn sync_kopia_manifest(app: &tauri::AppHandle, state: &AppStateWrapper) -> Result<()> {
    let _manifest_guard = manifest_update_lock().lock().await;
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
        AccountContext::capture(&guard)?
    };
    let snapshots = list_snapshots_from_repository(app, &context).await?;
    upload_manifest_with_retry(&context.api, &snapshots, None).await
}

#[cfg(test)]
mod tests {
    use super::{
        backup_reliability_policy_args, begin_operation, cancel_restore, classify_kopia_error,
        clear_restore_cancellation, database_content_object_from_value,
        ensure_restore_not_cancelled, execute_rollback_steps, expired_profile_snapshot_ids,
        parse_snapshot, repository_is_missing, try_begin_update,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn admitted_workflow_reuses_lease_when_maintenance_is_queued() {
        let gate = tokio::sync::RwLock::new(());
        let lease = super::begin_operation_in(&gate).unwrap();
        let writer = gate.write();
        tokio::pin!(writer);
        // Polling once queues the writer deterministically, without a sleep.
        assert!(futures_util::poll!(writer.as_mut()).is_pending());
        // This is the previous nested-begin_operation failure: Tokio reserves
        // admission for the queued writer even while the outer read is held.
        assert!(super::begin_operation_in(&gate).is_err());

        async fn nested_step(_lease: &super::EngineLease<'_>, completed: &mut Vec<u8>, step: u8) {
            tokio::task::yield_now().await;
            completed.push(step);
        }
        let mut completed = Vec::new();
        for step in 0..3 {
            nested_step(&lease, &mut completed, step).await;
            assert!(futures_util::poll!(writer.as_mut()).is_pending());
            assert!(super::begin_operation_in(&gate).is_err());
        }
        assert_eq!(completed, vec![0, 1, 2]);
        drop(lease);
        let maintenance = tokio::time::timeout(std::time::Duration::from_secs(1), writer)
            .await
            .unwrap();
        assert!(super::begin_operation_in(&gate).is_err());
        drop(maintenance);
        assert!(super::begin_operation_in(&gate).is_ok());
    }

    fn sleeping_stream() -> super::StreamSourceCommand {
        super::StreamSourceCommand {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "subprocess::tests::helper_fixture",
                "--nocapture",
            ]
            .iter()
            .map(Into::into)
            .collect(),
            env: vec![("SAVESTATE_TEST_HELPER".into(), "sleep".into())],
        }
    }

    fn sleeping_command() -> std::process::Command {
        let source = sleeping_stream();
        let mut command = std::process::Command::new(source.program);
        command.args(source.args).envs(source.env);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        command
    }

    #[tokio::test]
    async fn database_stream_backup_cancellation_stops_both_children() {
        let control = crate::backup_operations::BackupControl::fixture();
        let worker_control = Arc::clone(&control);
        let task = tokio::task::spawn_blocking(move || {
            super::run_stream_pipeline_commands(
                sleeping_stream(),
                sleeping_command(),
                &worker_control,
            )
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        control.cancel_fixture().await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert!(crate::backup_operations::is_cancelled(
            &result.err().unwrap()
        ));
    }

    #[tokio::test]
    async fn database_restore_cancellation_stops_both_children() {
        let key = "disposable-cancellation-restore";
        clear_restore_cancellation(key);
        let task = tokio::task::spawn_blocking(move || {
            let source = sleeping_stream();
            let target = super::StreamRestoreCommand {
                program: source.program,
                args: source.args,
                env: source.env,
            };
            super::run_database_restore_pipeline_commands(sleeping_command(), target, key)
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel_restore(key);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Restore cancelled"));
        clear_restore_cancellation(key);
    }

    #[tokio::test]
    async fn updater_cannot_reserve_engine_during_an_operation() {
        let operation = begin_operation().await.unwrap();
        assert!(try_begin_update().is_err());
        drop(operation);
        assert!(try_begin_update().is_ok());
    }

    #[test]
    fn cancellation_is_scoped_to_one_snapshot() {
        clear_restore_cancellation("snapshot-a");
        clear_restore_cancellation("snapshot-b");
        cancel_restore("snapshot-a");
        assert!(ensure_restore_not_cancelled("snapshot-a").is_err());
        assert!(ensure_restore_not_cancelled("snapshot-b").is_ok());
        clear_restore_cancellation("snapshot-a");
    }

    #[test]
    fn snapshot_create_json_contains_manifest_metadata() {
        let value = serde_json::json!({
            "id": "abc123",
            "source": { "path": "C:\\Users\\Test\\Pictures" },
            "startTime": "2026-08-17T18:00:00Z",
            "rootEntry": { "summ": { "size": 1024, "files": 3 } }
        });
        let snapshot = parse_snapshot(&value);
        assert_eq!(snapshot.id, "abc123");
        assert_eq!(snapshot.size, 1024);
        assert_eq!(snapshot.file_count, 3);
    }

    #[test]
    fn profile_retention_never_prunes_versions_moved_out_of_the_managed_folder() {
        let snapshot = |id: &str, profile_id: &str, folder: &str, version_number: u64| {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "sourcePath": "C:\\Screenshots",
                "startTime": format!("2026-08-{version_number:02}T10:00:00Z"),
                "size": 100,
                "fileCount": 1,
                "folder": folder,
                "profileId": profile_id,
                "profileName": "Screenshots",
                "trigger": "manual",
                "versionNumber": version_number
            }))
            .unwrap()
        };
        let expired = expired_profile_snapshot_ids(
            vec![
                snapshot("newest", "profile-a", "/Screenshots", 4),
                snapshot("middle", "profile-a", "/Screenshots", 3),
                snapshot("saved-elsewhere", "profile-a", "/Tested working DB", 2),
                snapshot("oldest", "profile-a", "/Screenshots", 1),
                snapshot("other-profile", "profile-b", "/Screenshots", 99),
            ],
            "profile-a",
            "/Screenshots",
            2,
        );
        assert_eq!(expired, vec!["oldest"]);
    }

    #[test]
    fn database_snapshot_metadata_and_stream_object_are_preserved() {
        let value = serde_json::json!({
            "id": "database-snapshot",
            "source": { "path": "-" },
            "startTime": "2026-08-24T09:00:00Z",
            "tags": {
                "tag:backup-kind": "database",
                "tag:database-profile": "profile-db"
            },
            "rootEntry": {
                "obj": "root-object",
                "summ": { "size": 2048, "files": 1 }
            }
        });
        let snapshot = parse_snapshot(&value);
        assert_eq!(snapshot.backup_kind, "database");
        assert_eq!(snapshot.database_profile_id.as_deref(), Some("profile-db"));
        assert_eq!(snapshot.root_object_id.as_deref(), Some("root-object"));

        let directory = serde_json::json!({
            "entries": [
                { "name": "metadata.json", "type": "f", "obj": "metadata-object" },
                {
                    "name": "savestate-database/profile-db/database.sql",
                    "type": "f",
                    "obj": "sql-object"
                }
            ]
        });
        assert_eq!(
            database_content_object_from_value(&directory).unwrap(),
            "sql-object"
        );
    }

    #[test]
    fn repository_creation_only_follows_an_explicit_missing_repository_error() {
        assert!(repository_is_missing(
            "repository not initialized in the provided storage"
        ));
        assert!(!repository_is_missing(
            "decrypt: unable to decrypt content: cipher: message authentication failed"
        ));
        assert!(!repository_is_missing("request timed out"));
    }

    #[test]
    fn existing_windows_repository_disables_legacy_retention_and_keeps_vss() {
        let args = backup_reliability_policy_args(false, true).unwrap();
        assert_eq!(
            args,
            vec![
                "policy",
                "set",
                "--global",
                "--keep-latest=2147483647",
                "--keep-hourly=0",
                "--keep-daily=0",
                "--keep-weekly=0",
                "--keep-monthly=0",
                "--keep-annual=0",
                "--enable-volume-shadow-copy=when-available",
            ]
        );
    }

    #[test]
    fn new_windows_repository_gets_compression_and_vss_defaults() {
        let args = backup_reliability_policy_args(true, true).unwrap();
        assert_eq!(
            args,
            vec![
                "policy",
                "set",
                "--global",
                "--keep-latest=2147483647",
                "--keep-hourly=0",
                "--keep-daily=0",
                "--keep-weekly=0",
                "--keep-monthly=0",
                "--keep-annual=0",
                "--compression=zstd",
                "--enable-volume-shadow-copy=when-available",
            ]
        );
    }

    #[test]
    fn existing_non_windows_repository_disables_legacy_global_retention() {
        assert_eq!(
            backup_reliability_policy_args(false, false).unwrap(),
            vec![
                "policy",
                "set",
                "--global",
                "--keep-latest=2147483647",
                "--keep-hourly=0",
                "--keep-daily=0",
                "--keep-weekly=0",
                "--keep-monthly=0",
                "--keep-annual=0",
            ]
        );
    }

    #[test]
    fn new_non_windows_repository_keeps_the_compression_default() {
        let args = backup_reliability_policy_args(true, false).unwrap();
        assert_eq!(
            args,
            vec![
                "policy",
                "set",
                "--global",
                "--keep-latest=2147483647",
                "--keep-hourly=0",
                "--keep-daily=0",
                "--keep-weekly=0",
                "--keep-monthly=0",
                "--keep-annual=0",
                "--compression=zstd",
            ]
        );
    }

    #[test]
    fn decryption_stack_traces_become_a_stable_support_error() {
        let error = classify_kopia_error(
            "snapshot delete",
            "unable to load manifest contents: decrypt: cipher: message authentication failed\nstack trace",
        );
        assert!(error.to_string().starts_with("REPOSITORY_KEY_MISMATCH:"));
        assert!(!error.to_string().contains("stack trace"));
    }

    #[tokio::test]
    async fn rollback_keeps_manifest_when_kopia_delete_fails() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let delete_calls = Arc::clone(&calls);
        let manifest_calls = Arc::clone(&calls);
        let result = execute_rollback_steps(
            true,
            move || async move {
                delete_calls.lock().unwrap().push("delete");
                Err(anyhow::anyhow!("delete failed"))
            },
            move || async move {
                manifest_calls.lock().unwrap().push("manifest");
                Ok(())
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), vec!["delete"]);
    }

    #[tokio::test]
    async fn rollback_removes_manifest_only_after_kopia_snapshot() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let delete_calls = Arc::clone(&calls);
        let manifest_calls = Arc::clone(&calls);
        execute_rollback_steps(
            true,
            move || async move {
                delete_calls.lock().unwrap().push("delete");
                Ok(())
            },
            move || async move {
                manifest_calls.lock().unwrap().push("manifest");
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["delete", "manifest"]);
    }
}
