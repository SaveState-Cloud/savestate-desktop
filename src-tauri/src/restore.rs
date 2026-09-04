use crate::state::AppStateWrapper;
use std::path::{Path, PathBuf};

// Retain the historical encryption-format regression fixture, not the unused
// legacy archive extraction engine. All registered restore commands use Kopia.
#[cfg(test)]
use aes_gcm::{
    aead::{Aead, KeyInit, Nonce},
    Aes256Gcm,
};
#[cfg(test)]
use anyhow::{anyhow, Result};

#[cfg(test)]
pub fn decrypt_chunk_v2(
    encrypted_chunk: &[u8],
    base_nonce: &[u8; 12],
    chunk_index: u32,
    master_key: &[u8; 32],
) -> Result<Vec<u8>> {
    let mut nonce_bytes = *base_nonce;
    let idx_bytes = chunk_index.to_le_bytes();
    for i in 0..4 {
        nonce_bytes[i] ^= idx_bytes[i];
    }

    let cipher =
        Aes256Gcm::new_from_slice(master_key).map_err(|e| anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::<Aes256Gcm>::from(nonce_bytes);

    cipher
        .decrypt(&nonce, encrypted_chunk)
        .map_err(|_| anyhow!("V2 Chunk Decryption failed — part {}", chunk_index))
}

#[tauri::command]
pub async fn cmd_restore_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    key: String,
    filename: String,
    destination: String,
) -> std::result::Result<(), String> {
    let engine = crate::kopia::begin_operation()
        .await
        .map_err(|error| error.to_string())?;
    let restore_root = PathBuf::from(&destination);
    if !restore_root.is_dir() {
        return Err(format!(
            "Destination directory does not exist: {}",
            destination
        ));
    }

    let safe_name = Path::new(&filename)
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("restored-backup"));
    let display_name = safe_name.to_string_lossy().to_string();
    let final_dest = restore_root.join(safe_name);
    if final_dest.exists() {
        return Err(format!(
            "Restore destination already exists: {}. Choose an empty destination folder.",
            final_dest.display()
        ));
    }

    // Restore into an app-owned staging directory. A cancelled or failed
    // operation is deleted before it becomes visible as a completed restore.
    let staging = restore_root.join(format!(".savestate-restore-{}", uuid::Uuid::new_v4()));
    let api = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {}", error))?;
        guard.api.clone()
    };
    let restore_result = crate::kopia::restore_snapshot_with_lease(
        &app,
        &engine,
        state.inner(),
        &key,
        &staging.to_string_lossy(),
    )
    .await;

    let result = match restore_result {
        Ok(()) => std::fs::rename(&staging, &final_dest).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging);
            format!("Failed to finalize restore: {}", error)
        }),
        Err(error) => {
            if staging.starts_with(&restore_root)
                && staging
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with(".savestate-restore-"))
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_dir_all(&staging);
            }
            Err(error.to_string())
        }
    };

    match &result {
        Ok(()) => {
            crate::notifications::send_backup_notification(
                &api,
                "restore_success",
                &display_name,
                "Restore completed successfully.",
            )
            .await
        }
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "restore_failure",
                &display_name,
                "Restore failed. Open SaveState Vault for details.",
            )
            .await
        }
    }
    result
}

#[tauri::command]
pub fn cmd_cancel_restore(key: String) -> std::result::Result<(), String> {
    crate::kopia::cancel_restore(&key);
    Ok(())
}

#[tauri::command]
pub async fn cmd_list_backups(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let snapshots = crate::kopia::list_snapshots(&app, state.inner())
        .await
        .map_err(|e| e.to_string())?;

    let backups: Vec<serde_json::Value> = snapshots
        .into_iter()
        .map(|snap| {
            let filename = if snap.backup_kind == "database" {
                format!(
                    "{} (Database)",
                    snap.database_profile_name
                        .as_deref()
                        .unwrap_or("Database backup")
                )
            } else {
                std::path::Path::new(&snap.source_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| snap.id.clone())
            };

            serde_json::json!({
                "key": snap.id,
                "filename": filename,
                "folder": snap.folder,
                "size": snap.size,
                "sizeFormatted": format_bytes(snap.size),
                "lastModified": snap.start_time,
                "backupKind": snap.backup_kind,
                "databaseProfileId": snap.database_profile_id,
                "profileId": snap.profile_id,
                "profileName": snap.profile_name,
                "trigger": snap.trigger,
                "versionNumber": snap.version_number,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "backups": backups,
        "folders": [],
        "currentFolder": "/",
        "count": backups.len(),
    }))
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{:.2} {}", size, UNITS[unit])
}

#[tauri::command]
pub async fn cmd_get_backup_manifest(
    _state: tauri::State<'_, AppStateWrapper>,
    _key: String,
) -> std::result::Result<serde_json::Value, String> {
    Ok(serde_json::json!({ "files": [] }))
}

#[tauri::command]
pub async fn cmd_restore_selected_files(
    _app: tauri::AppHandle,
    _state: tauri::State<'_, AppStateWrapper>,
    _key: String,
    _filename: String,
    _destination: String,
    _selected_paths: Vec<String>,
) -> std::result::Result<(), String> {
    Err(
        "Selective restore is not supported for deduplicated backups yet. Use Restore All instead."
            .to_string(),
    )
}
