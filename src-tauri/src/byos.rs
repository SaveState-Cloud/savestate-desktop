//! Customer-owned S3-compatible repositories. Provider credentials never leave
//! this computer: SQLite contains only destination metadata and Windows
//! Credential Manager contains the access key pair. BYOS never calls the
//! managed repository gateway, manifest, storage quota, or restore meter.

use crate::api::{RepoSession, SaveStateClient};
use crate::backup_operations::{self, AccountContext, BackupControl};
use crate::db::{self, BackupProfile};
use crate::kopia::{self, KopiaSnapshot};
use crate::state::AppStateWrapper;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tauri::Emitter;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vault {
    pub id: String,
    #[serde(skip_serializing)]
    owner_account: String,
    pub label: String,
    pub provider: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewVault {
    label: String,
    provider: String,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByosEntitlement {
    enabled: bool,
    max_vaults: u32,
}

#[derive(Serialize, Deserialize)]
struct VaultCredentials {
    access_key_id: String,
    secret_access_key: String,
}

fn credential_entry(owner: &str, vault_id: &str) -> Result<keyring::v1::Entry> {
    let owner_hash = hex::encode(Sha256::digest(owner.as_bytes()));
    keyring::v1::Entry::new(
        &crate::runtime_storage::credential_service("SaveState BYOS"),
        &format!("{}-{}", &owner_hash[..24], vault_id),
    )
    .context("Windows Credential Manager is unavailable")
}

fn validate(input: NewVault, owner_account: String) -> Result<(Vault, VaultCredentials)> {
    let label = input.label.trim();
    if label.is_empty() || label.len() > 80 {
        bail!("Give this storage destination a name of up to 80 characters");
    }
    if !matches!(input.provider.as_str(), "s3" | "b2" | "r2" | "minio") {
        bail!("Choose S3, Backblaze B2, Cloudflare R2, or MinIO");
    }
    let url = reqwest::Url::parse(input.endpoint.trim())
        .context("Enter a valid endpoint URL, including https://")?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("The endpoint must be a host URL with no path, login, or query");
    }
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback && input.provider == "minio")
    {
        bail!("Use HTTPS. Plain HTTP is allowed only for MinIO on this PC");
    }
    let bucket = input.bucket.trim();
    if bucket.is_empty()
        || bucket.len() > 63
        || !bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("Enter a valid S3 bucket name");
    }
    let region = input.region.trim();
    if region.is_empty()
        || region.len() > 64
        || !region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("Enter the bucket region (R2 commonly uses auto)");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let prefix = if input.prefix.trim().is_empty() {
        // A stable default lets the same account reconnect this bucket on a
        // replacement PC without a server-side copy of its storage details.
        let mut hasher = Sha256::new();
        hasher.update(owner_account.as_bytes());
        hasher.update([0]);
        hasher.update(url.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(bucket.as_bytes());
        format!("savestate/{}/", &hex::encode(hasher.finalize())[..24])
    } else {
        let value = input.prefix.trim().trim_matches('/');
        if value.is_empty()
            || value.len() > 120
            || value
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
        {
            bail!("Use a simple storage prefix with letters, numbers, dashes, or underscores");
        }
        format!("{value}/")
    };
    if input.access_key_id.trim().is_empty() || input.secret_access_key.is_empty() {
        bail!("Enter both the access key ID and secret key");
    }
    Ok((
        Vault {
            id,
            owner_account,
            label: label.to_string(),
            provider: input.provider,
            endpoint: url.to_string().trim_end_matches('/').to_string(),
            region: region.to_string(),
            bucket: bucket.to_string(),
            prefix,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        VaultCredentials {
            access_key_id: input.access_key_id,
            secret_access_key: input.secret_access_key,
        },
    ))
}

fn vault_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vault> {
    Ok(Vault {
        id: row.get(0)?,
        owner_account: row.get(1)?,
        label: row.get(2)?,
        provider: row.get(3)?,
        endpoint: row.get(4)?,
        region: row.get(5)?,
        bucket: row.get(6)?,
        prefix: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn get_vault(state: &AppStateWrapper, owner: &str, id: &str) -> Result<Vault> {
    let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
    guard.db.query_row(
        "SELECT id, owner_account, label, provider, endpoint, region, bucket, prefix, created_at
         FROM byos_vaults WHERE id = ?1 AND owner_account = ?2",
        params![id, owner],
        vault_from_row,
    ).optional()?.ok_or_else(|| anyhow!("This storage destination is not connected on this PC"))
}

pub(crate) fn require_vault(state: &AppStateWrapper, owner: &str, id: &str) -> Result<Vault> {
    get_vault(state, owner, id)
}

async fn require_entitlement(api: &SaveStateClient) -> Result<u32> {
    let entitlements = api
        .get_entitlements()
        .await
        .context("Could not verify BYOS access; try again when the API is available")?;
    if !entitlements.byos_enabled {
        bail!("BYOS is available on an active Pro or Ultra plan");
    }
    Ok(entitlements.max_byos_vaults)
}

pub(crate) async fn verify_entitlement(api: &SaveStateClient) -> Result<()> {
    require_entitlement(api).await.map(|_| ())
}

fn session(vault: &Vault, credentials: VaultCredentials, mode: &str) -> RepoSession {
    RepoSession {
        mode: mode.to_string(),
        bucket: vault.bucket.clone(),
        prefix: vault.prefix.clone(),
        endpoint: vault.endpoint.clone(),
        endpoint_host: None,
        region: vault.region.clone(),
        access_key_id: credentials.access_key_id,
        secret_access_key: credentials.secret_access_key,
        expires_in: 0,
    }
}

fn read_session(vault: &Vault, mode: &str) -> Result<RepoSession> {
    let bytes = credential_entry(&vault.owner_account, &vault.id)?
        .get_secret()
        .context("Storage credentials are missing on this PC. Reconnect the destination")?;
    let credentials: VaultCredentials = serde_json::from_slice(&bytes)
        .context("Stored storage credentials are unreadable; reconnect the destination")?;
    Ok(session(vault, credentials, mode))
}

fn safe_output(output: &std::process::Output, action: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        // S3 endpoints can echo request details, so never return raw stderr to
        // the UI, logs, or engine telemetry.
        bail!("BYOS_{action}_FAILED: Check the destination and credentials, then retry")
    }
}

async fn connect(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
    create_if_missing: bool,
    cancellation: Option<&Arc<BackupControl>>,
) -> Result<()> {
    let app = app.clone();
    let session = session.clone();
    let password = password.to_string();
    let cancellation = cancellation.cloned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut args = vec!["repository".into(), "connect".into()];
        args.extend(kopia::s3_connect_args(&session));
        let output = kopia::run_kopia_for_backup(
            &app,
            &args,
            Some(&password),
            Some(&session),
            cancellation.as_ref(),
        )?;
        let mut new_repository = false;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if create_if_missing && kopia::repository_is_missing(&stderr) {
                let mut create = vec!["repository".into(), "create".into()];
                create.extend(kopia::s3_connect_args(&session));
                let output = kopia::run_kopia_for_backup(
                    &app,
                    &create,
                    Some(&password),
                    Some(&session),
                    cancellation.as_ref(),
                )?;
                safe_output(&output, "CREATE")?;
                new_repository = true;
            } else {
                safe_output(&output, "CONNECT")?;
            }
        }
        // Never rewrite the policy of a customer-owned Kopia repository we
        // merely connected to. It may contain backups from other tools.
        if new_repository {
            if let Some(policy) =
                kopia::backup_reliability_policy_args(new_repository, cfg!(windows))
            {
                let output = kopia::run_kopia_for_backup(
                    &app,
                    &policy,
                    Some(&password),
                    Some(&session),
                    cancellation.as_ref(),
                )?;
                safe_output(&output, "POLICY")?;
            }
        }
        Ok(())
    })
    .await
    .context("BYOS connection task stopped unexpectedly")?
}

async fn validate_provider(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
) -> Result<()> {
    let app = app.clone();
    let session = session.clone();
    let password = password.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec!["repository".into(), "validate-provider".into()];
        let output = kopia::run_kopia(&app, &args, Some(&password), Some(&session))?;
        safe_output(&output, "PROVIDER_VALIDATION")
    })
    .await
    .context("BYOS provider validation stopped unexpectedly")?
}

#[tauri::command]
pub async fn cmd_byos_entitlements(
    state: tauri::State<'_, AppStateWrapper>,
) -> Result<ByosEntitlement, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.account_scope().ok_or("Sign in first")?;
        guard.api.clone()
    };
    let value = api.get_entitlements().await.map_err(|e| e.to_string())?;
    Ok(ByosEntitlement {
        enabled: value.byos_enabled,
        max_vaults: value.max_byos_vaults,
    })
}

#[tauri::command]
pub async fn cmd_byos_list_vaults(
    state: tauri::State<'_, AppStateWrapper>,
) -> Result<Vec<Vault>, String> {
    let guard = state.0.lock().map_err(|e| e.to_string())?;
    let owner = guard.account_scope().ok_or("Sign in first")?;
    let mut stmt = guard.db.prepare(
        "SELECT id, owner_account, label, provider, endpoint, region, bucket, prefix, created_at
         FROM byos_vaults WHERE owner_account = ?1 ORDER BY created_at DESC"
    ).map_err(|e| e.to_string())?;
    let vaults = stmt
        .query_map(params![owner], vault_from_row)
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok(vaults)
}

#[tauri::command]
pub async fn cmd_byos_add_vault(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    input: NewVault,
) -> Result<Vault, String> {
    let context = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        AccountContext::capture(&guard).map_err(|e| e.to_string())?
    };
    let max_vaults = require_entitlement(&context.api)
        .await
        .map_err(|e| e.to_string())?;
    let (vault, credentials) =
        validate(input, context.account_scope.clone()).map_err(|e| e.to_string())?;
    let count: u32 = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        guard
            .db
            .query_row(
                "SELECT COUNT(*) FROM byos_vaults WHERE owner_account = ?1",
                params![context.account_scope],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?
    };
    if count >= max_vaults {
        return Err(format!(
            "This account has reached its {max_vaults}-destination safety limit"
        ));
    }
    {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        let duplicate: u32 = guard.db.query_row(
            "SELECT COUNT(*) FROM byos_vaults WHERE owner_account = ?1 AND endpoint = ?2 AND bucket = ?3 AND prefix = ?4",
            params![vault.owner_account, vault.endpoint, vault.bucket, vault.prefix],
            |row| row.get(0),
        ).map_err(|e| e.to_string())?;
        if duplicate > 0 {
            return Err("This bucket and prefix are already connected on this PC".into());
        }
    }
    let session = session(&vault, credentials, "backup");
    let _engine = kopia::begin_operation().await.map_err(|e| e.to_string())?;
    context
        .ensure_current(state.inner())
        .map_err(|e| e.to_string())?;
    connect(&app, &session, &context.repository_password, true, None)
        .await
        .map_err(|e| e.to_string())?;
    validate_provider(&app, &session, &context.repository_password)
        .await
        .map_err(|e| e.to_string())?;
    context
        .ensure_current(state.inner())
        .map_err(|e| e.to_string())?;
    let credentials = VaultCredentials {
        access_key_id: session.access_key_id,
        secret_access_key: session.secret_access_key,
    };
    let entry = credential_entry(&vault.owner_account, &vault.id).map_err(|e| e.to_string())?;
    entry
        .set_secret(&serde_json::to_vec(&credentials).map_err(|e| e.to_string())?)
        .map_err(|e| format!("Could not securely save storage credentials: {e}"))?;
    let result = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.db.execute(
            "INSERT INTO byos_vaults (id, owner_account, label, provider, endpoint, region, bucket, prefix, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![vault.id, vault.owner_account, vault.label, vault.provider, vault.endpoint, vault.region, vault.bucket, vault.prefix, vault.created_at],
        )
    };
    if let Err(error) = result {
        let _ = entry.delete_credential();
        return Err(format!("Could not save the destination: {error}"));
    }
    Ok(vault)
}

#[tauri::command]
pub async fn cmd_byos_remove_vault(
    state: tauri::State<'_, AppStateWrapper>,
    vault_id: String,
) -> Result<(), String> {
    let _engine = kopia::try_begin_update().map_err(|e| e.to_string())?;
    let owner = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.account_scope().ok_or("Sign in first")?
    };
    let vault = get_vault(state.inner(), &owner, &vault_id).map_err(|e| e.to_string())?;
    let guard = state.0.lock().map_err(|e| e.to_string())?;
    let profiles: u32 = guard
        .db
        .query_row(
            "SELECT COUNT(*) FROM backup_profiles WHERE owner_account = ?1 AND vault_id = ?2",
            params![owner, vault_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if profiles > 0 {
        return Err("Move or delete profiles using this destination first".into());
    }
    let entry = credential_entry(&vault.owner_account, &vault.id).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::v1::Error::NoEntry) => {}
        Err(error) => {
            return Err(format!(
                "Could not remove local storage credentials: {error}"
            ))
        }
    }
    guard
        .db
        .execute(
            "DELETE FROM byos_vaults WHERE id = ?1 AND owner_account = ?2",
            params![vault_id, owner],
        )
        .map_err(|e| e.to_string())?;
    // The remote bucket and its backups remain the customer's property.
    Ok(())
}

async fn checked_session(
    state: &AppStateWrapper,
    context: &AccountContext,
    vault_id: &str,
    mode: &str,
) -> Result<RepoSession> {
    // A former subscriber can still recover files from their own bucket.
    // The active plan is required only for writes/new destinations.
    if mode == "backup" {
        verify_entitlement(&context.api).await?;
    }
    context.ensure_current(state)?;
    let vault = get_vault(state, &context.account_scope, vault_id)?;
    read_session(&vault, mode)
}

async fn list_snapshots(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &AccountContext,
    vault_id: &str,
) -> Result<Vec<KopiaSnapshot>> {
    let session = checked_session(state, context, vault_id, "restore").await?;
    let _engine = kopia::begin_operation().await?;
    connect(app, &session, &context.repository_password, false, None).await?;
    list_connected_snapshots(app, &session, &context.repository_password).await
}

async fn list_connected_snapshots(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
) -> Result<Vec<KopiaSnapshot>> {
    let app = app.clone();
    let session = session.clone();
    let password = password.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<KopiaSnapshot>> {
        let args = vec![
            "snapshot".into(),
            "list".into(),
            "--all".into(),
            "--json".into(),
        ];
        let output = kopia::run_kopia(&app, &args, Some(&password), Some(&session))?;
        safe_output(&output, "LIST")?;
        let values: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("BYOS snapshot metadata is unreadable")?;
        let items = values
            .as_array()
            .ok_or_else(|| anyhow!("BYOS snapshot list is invalid"))?;
        Ok(items.iter().map(kopia::parse_snapshot).collect())
    })
    .await
    .context("BYOS list task stopped unexpectedly")?
}

#[tauri::command]
pub async fn cmd_byos_list_snapshots(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    vault_id: String,
) -> Result<Vec<KopiaSnapshot>, String> {
    let context = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        AccountContext::capture(&guard).map_err(|e| e.to_string())?
    };
    list_snapshots(&app, state.inner(), &context, &vault_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_byos_restore(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    vault_id: String,
    snapshot_id: String,
    target_path: String,
) -> Result<String, String> {
    let parent = std::path::Path::new(&target_path);
    if !parent.is_dir() {
        return Err("Choose an existing restore folder".into());
    }
    let folder_name = format!(
        "SaveState-Restore-{}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let restore_path = parent.join(folder_name);
    let context = {
        let guard = state.0.lock().map_err(|e| e.to_string())?;
        AccountContext::capture(&guard).map_err(|e| e.to_string())?
    };
    let snapshots = list_snapshots(&app, state.inner(), &context, &vault_id)
        .await
        .map_err(|e| e.to_string())?;
    if !snapshots.iter().any(|snapshot| snapshot.id == snapshot_id) {
        return Err("That restore point is not in this destination".into());
    }
    let session = checked_session(state.inner(), &context, &vault_id, "restore")
        .await
        .map_err(|e| e.to_string())?;
    let _engine = kopia::begin_operation().await.map_err(|e| e.to_string())?;
    connect(&app, &session, &context.repository_password, false, None)
        .await
        .map_err(|e| e.to_string())?;
    context
        .ensure_current(state.inner())
        .map_err(|e| e.to_string())?;
    // Create the unique destination atomically before Kopia writes anything.
    // Never permit a restore to overwrite a folder that appeared meanwhile.
    std::fs::create_dir(&restore_path)
        .map_err(|e| format!("Could not create a new restore folder: {e}"))?;
    let password = context.repository_password.clone();
    let restore_path_text = restore_path.to_string_lossy().to_string();
    tokio::task::spawn_blocking(move || -> Result<String> {
        let args = vec![
            "restore".into(),
            snapshot_id,
            restore_path_text.clone(),
            "--no-progress".into(),
        ];
        let output = kopia::run_kopia(&app, &args, Some(&password), Some(&session))?;
        safe_output(&output, "RESTORE")?;
        Ok(restore_path_text)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

pub(crate) async fn backup_profile(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: AccountContext,
    profile: &BackupProfile,
    trigger: &'static str,
) -> Result<String> {
    let vault_id = profile
        .vault_id
        .as_deref()
        .ok_or_else(|| anyhow!("Profile has no BYOS destination"))?;
    let session = checked_session(state, &context, vault_id, "backup").await?;
    let source = std::path::Path::new(&profile.source_path);
    if !source.exists() {
        bail!("Source folder is missing: {}", profile.source_path);
    }
    let operation = backup_operations::begin_with_context(state, context, profile.name.clone())?;
    let _engine = kopia::begin_operation().await?;
    let result: Result<String> = async {
        connect(
            app,
            &session,
            &operation.context.repository_password,
            true,
            Some(&operation.control),
        )
        .await?;
        operation.ensure_not_cancelled()?;
        let app_c = app.clone();
        let session_c = session.clone();
        let password = operation.context.repository_password.clone();
        let control = Arc::clone(&operation.control);
        let profile_id = profile.id.clone();
        let path = profile.source_path.clone();
        let trigger = trigger.to_string();
        let output = tokio::task::spawn_blocking(move || {
            let args = vec![
                "snapshot".into(),
                "create".into(),
                path,
                "--tags=backup-kind:files".into(),
                format!("--tags=savestate-profile:{profile_id}"),
                format!("--tags=savestate-trigger:{trigger}"),
                "--json".into(),
                "--no-progress".into(),
            ];
            kopia::run_kopia_for_backup(
                &app_c,
                &args,
                Some(&password),
                Some(&session_c),
                Some(&control),
            )
        })
        .await
        .context("BYOS backup task stopped unexpectedly")??;
        safe_output(&output, "BACKUP")?;
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("BYOS snapshot metadata is unreadable")?;
        let snapshot = kopia::parse_snapshot(&value);
        if snapshot.id.is_empty() {
            bail!("BYOS backup did not return a restore point ID");
        }
        if let Err(cancelled) = operation.control.mark_committed().await {
            delete_snapshot(
                app,
                &session,
                &operation.context.repository_password,
                &snapshot.id,
            )
            .await
            .context(
                "Sign-out stopped the backup, but the new restore point could not be removed",
            )?;
            return Err(cancelled);
        }
        let now = chrono::Utc::now().to_rfc3339();
        let next = crate::profiles::compute_next_run(profile.schedule.as_deref());
        if let Err(error) = (|| -> Result<()> {
            let guard = state.0.lock().map_err(|e| anyhow!("Lock: {e}"))?;
            db::update_profile_run_times(
                &guard.db,
                &profile.id,
                operation.account_scope(),
                &now,
                next.as_deref(),
            )
        })() {
            // The encrypted snapshot is already committed to the owner's
            // bucket. Never report this as a failed backup or retry it as one.
            eprintln!("BYOS backup committed but local schedule update failed: {error}");
        }
        if profile.retention > 0 {
            if let Err(error) = prune_profile(app, &operation.context, &session, profile).await {
                eprintln!("BYOS backup committed but retention cleanup failed: {error:#}");
            }
        }
        let _ = app.emit(
            "backup-progress",
            crate::backup::BackupProgress {
                id: snapshot.id.clone(),
                stage: "done".into(),
                progress: 1.0,
                message: "BYOS backup complete".into(),
            },
        );
        Ok(snapshot.id)
    }
    .await;
    operation.finish_tracking().await;
    result
}

async fn delete_snapshot(
    app: &tauri::AppHandle,
    session: &RepoSession,
    password: &str,
    id: &str,
) -> Result<()> {
    let app = app.clone();
    let session = session.clone();
    let password = password.to_string();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let args = vec!["snapshot".into(), "delete".into(), id, "--delete".into()];
        let output = kopia::run_kopia(&app, &args, Some(&password), Some(&session))?;
        safe_output(&output, "DELETE")
    })
    .await
    .context("BYOS delete task stopped unexpectedly")?
}

async fn prune_profile(
    app: &tauri::AppHandle,
    context: &AccountContext,
    session: &RepoSession,
    profile: &BackupProfile,
) -> Result<()> {
    // The caller already holds the engine lease and has connected this
    // repository. Do not acquire another lease while a writer is waiting.
    let snapshots = list_connected_snapshots(app, session, &context.repository_password).await?;
    let mut own: Vec<_> = snapshots
        .into_iter()
        .filter(|item| item.profile_id.as_deref() == Some(&profile.id))
        .collect();
    own.sort_by(|a, b| b.start_time.cmp(&a.start_time));
    for item in own.into_iter().skip(profile.retention.max(0) as usize) {
        delete_snapshot(app, session, &context.repository_password, &item.id).await?;
    }
    Ok(())
}

pub(crate) async fn delete_profile_snapshots(
    app: &tauri::AppHandle,
    state: &AppStateWrapper,
    context: &AccountContext,
    profile: &BackupProfile,
) -> Result<()> {
    let vault_id = profile
        .vault_id
        .as_deref()
        .ok_or_else(|| anyhow!("Profile has no destination"))?;
    let session = checked_session(state, context, vault_id, "delete").await?;
    let snapshots = list_snapshots(app, state, context, vault_id).await?;
    let _engine = kopia::begin_operation().await?;
    connect(app, &session, &context.repository_password, false, None).await?;
    for item in snapshots
        .into_iter()
        .filter(|item| item.profile_id.as_deref() == Some(&profile.id))
    {
        delete_snapshot(app, &session, &context.repository_password, &item.id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(endpoint: &str) -> NewVault {
        NewVault {
            label: "Home bucket".into(),
            provider: "minio".into(),
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: "backup".into(),
            prefix: "".into(),
            access_key_id: "test-key".into(),
            secret_access_key: "test-secret".into(),
        }
    }

    #[test]
    fn storage_credentials_are_never_in_serialized_vault_metadata() {
        let (vault, credentials) =
            validate(input("http://localhost:9000"), "owner".into()).unwrap();
        let json = serde_json::to_string(&vault).unwrap();
        assert!(!json.contains(&credentials.access_key_id));
        assert!(!json.contains(&credentials.secret_access_key));
        assert!(!json.contains("owner"));
        assert!(vault.prefix.starts_with("savestate/"));
        let (again, _) = validate(input("http://localhost:9000"), "owner".into()).unwrap();
        assert_eq!(vault.prefix, again.prefix);
    }

    #[test]
    fn default_prefix_is_stable_for_reconnection_and_account_scoped() {
        let (first, _) = validate(
            input("http://127.0.0.1:9000"),
            "a@example.com::service-1".into(),
        )
        .unwrap();
        let (again, _) = validate(
            input("http://127.0.0.1:9000"),
            "a@example.com::service-1".into(),
        )
        .unwrap();
        let (other, _) = validate(
            input("http://127.0.0.1:9000"),
            "b@example.com::service-1".into(),
        )
        .unwrap();
        assert_eq!(first.prefix, again.prefix);
        assert_ne!(first.prefix, other.prefix);
    }

    #[test]
    fn plaintext_remote_endpoints_are_rejected() {
        assert!(validate(input("http://storage.example.com"), "owner".into()).is_err());
        assert!(validate(
            input("https://user:password@storage.example.com"),
            "owner".into()
        )
        .is_err());
        assert!(validate(input("https://storage.example.com/path"), "owner".into()).is_err());
    }
}
