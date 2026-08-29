use crate::api::SaveStateClient;
use crate::state::AppStateWrapper;
use aes_gcm::{
    aead::{Aead, KeyInit, Nonce},
    Aes256Gcm,
};
use anyhow::{anyhow, Context, Result};
use bytes::Buf;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use tar::Archive;
use tauri::Emitter;

pub struct ChannelReader {
    pub rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pub buffer: Vec<u8>,
    pub pos: usize,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buffer.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buffer = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }

        let available = self.buffer.len() - self.pos;
        let to_copy = std::cmp::min(available, buf.len());
        buf[..to_copy].copy_from_slice(&self.buffer[self.pos..self.pos + to_copy]);
        self.pos += to_copy;
        Ok(to_copy)
    }
}

// ────────────────────────────────────────────────────────────────────
// Constants (must match backup.rs)
// ────────────────────────────────────────────────────────────────────

const NONCE_LEN: usize = 12;

// ────────────────────────────────────────────────────────────────────
// Restore progress event payload
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct RestoreProgress {
    pub key: String,
    pub stage: String, // "downloading", "decrypting", "decompressing", "extracting", "done", "error"
    pub progress: f64,
    pub message: String,
}

// ────────────────────────────────────────────────────────────────────
// Crypto helpers — uses master_key directly (no Argon2 derivation)
// ────────────────────────────────────────────────────────────────────

/// Decrypt data encrypted with `encrypt_blob` from backup.rs.
/// Input format: `[12-byte nonce][ciphertext + tag]`
pub fn decrypt_blob(encrypted: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>> {
    if encrypted.len() < NONCE_LEN + 16 {
        return Err(anyhow!("Encrypted data too short"));
    }

    let nonce_bytes = &encrypted[..NONCE_LEN];
    let ciphertext = &encrypted[NONCE_LEN..];

    let cipher =
        Aes256Gcm::new_from_slice(master_key).map_err(|e| anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::<Aes256Gcm>::try_from(nonce_bytes)
        .map_err(|_| anyhow!("Encrypted data has an invalid nonce length"))?;

    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| anyhow!("Decryption failed — wrong key or corrupted data"))
}

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

fn decompress_data(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(std::io::Cursor::new(data)).context("Zstd decompression failed")
}

// ────────────────────────────────────────────────────────────────────
// Detect whether decompressed data is a tar archive
// ────────────────────────────────────────────────────────────────────

fn is_tar_archive(data: &[u8]) -> bool {
    // Tar magic number is at offset 257: "ustar"
    if data.len() > 262 {
        &data[257..262] == b"ustar"
    } else {
        false
    }
}

fn extract_tar(data: &[u8], dest: &Path) -> Result<()> {
    let mut archive = Archive::new(std::io::Cursor::new(data));
    archive
        .unpack(dest)
        .context("Failed to extract tar archive")?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Core restore pipeline
// ────────────────────────────────────────────────────────────────────

async fn run_restore_inner(
    app_handle: &tauri::AppHandle,
    api: SaveStateClient,
    master_key: [u8; 32],
    key: &str,
    dest: &Path,
    original_filename: &str,
    selected_paths: Option<&[String]>,
) -> Result<()> {
    // 1. Get presigned download URL
    emit_restore_progress(app_handle, key, "downloading", 0.1, "Getting download URL…");
    let download_resp = api.presign_download(key).await?;

    emit_restore_progress(app_handle, key, "downloading", 0.3, "Downloading…");
    let resp = api
        .download_stream_from_presigned_url(&download_resp.download_url)
        .await?;

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();

    let mut initial_buffer = bytes::BytesMut::new();
    while initial_buffer.len() < 24 {
        if let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed to read stream chunk")?;
            initial_buffer.extend_from_slice(&chunk);
        } else {
            break;
        }
    }

    // ── V2 STREAMING RESTORE ──
    if initial_buffer.len() >= 24 && &initial_buffer[0..12] == crate::backup::V2_MAGIC {
        emit_restore_progress(
            app_handle,
            key,
            "decrypting",
            0.5,
            "Decrypting (V2 Streaming)…",
        );

        let mut base_nonce = [0u8; 12];
        base_nonce.copy_from_slice(&initial_buffer[12..24]);
        initial_buffer.advance(24); // Remove MAGIC + NONCE

        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let dest_clone = dest.to_path_buf();
        let selected_paths_clone: Option<Vec<String>> = selected_paths.map(|s| s.to_vec());

        let stream_task = tokio::task::spawn_blocking(move || -> Result<()> {
            let reader = ChannelReader {
                rx,
                buffer: Vec::new(),
                pos: 0,
            };
            let decoder = zstd::Decoder::new(reader)?;
            let mut archive = Archive::new(decoder);

            if let Some(paths) = selected_paths_clone {
                let entries = archive.entries().context("Failed to read tar entries")?;
                for entry_result in entries {
                    let mut entry = entry_result.context("Failed to read tar entry")?;
                    let entry_path = entry.path()?.to_string_lossy().to_string();
                    let normalized = entry_path.replace('\\', "/");

                    let should_extract = paths.iter().any(|sel| {
                        let sel_norm = sel.replace('\\', "/");
                        normalized == sel_norm || normalized.starts_with(&format!("{}/", sel_norm))
                    });

                    if should_extract {
                        let out_path = dest_clone.join(&entry_path);
                        if let Some(parent) = out_path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        if entry.header().entry_type().is_dir() {
                            std::fs::create_dir_all(&out_path)?;
                        } else {
                            entry.unpack(&out_path)?;
                        }
                    }
                }
            } else {
                archive
                    .unpack(&dest_clone)
                    .context("Failed to unpack archive")?;
            }
            Ok(())
        });

        let mut part_number = 1;
        let encrypted_chunk_size = crate::backup::CHUNK_SIZE_PLAINTEXT + 16; // Tag is 16 bytes

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed to read stream chunk")?;
            initial_buffer.extend_from_slice(&chunk);

            while initial_buffer.len() >= encrypted_chunk_size {
                let encrypted_chunk = initial_buffer.split_to(encrypted_chunk_size);
                let decrypted =
                    decrypt_chunk_v2(&encrypted_chunk, &base_nonce, part_number, &master_key)?;
                tx.send(decrypted)?;
                part_number += 1;
            }
        }

        if !initial_buffer.is_empty() {
            let decrypted =
                decrypt_chunk_v2(&initial_buffer, &base_nonce, part_number, &master_key)?;
            tx.send(decrypted)?;
        }
        drop(tx);

        stream_task.await??;
        emit_restore_progress(app_handle, key, "done", 1.0, "Restore complete!");
        return Ok(());
    }

    // ── V1 LEGACY RESTORE (In-Memory) ──
    emit_restore_progress(
        app_handle,
        key,
        "downloading",
        0.4,
        "Downloading (V1 Legacy)…",
    );
    let mut encrypted = initial_buffer.to_vec();
    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.context("Failed to read stream chunk")?;
        encrypted.extend_from_slice(&chunk);
    }

    emit_restore_progress(
        app_handle,
        key,
        "decrypting",
        0.5,
        "Decrypting (V1 Legacy)…",
    );
    let compressed = decrypt_blob(&encrypted, &master_key)?;

    emit_restore_progress(app_handle, key, "decompressing", 0.7, "Decompressing…");
    let raw_data = decompress_data(&compressed)?;

    if let Some(paths) = selected_paths {
        if is_tar_archive(&raw_data) {
            emit_restore_progress(
                app_handle,
                key,
                "extracting",
                0.85,
                "Extracting selected files…",
            );
            let mut archive = Archive::new(std::io::Cursor::new(&raw_data));
            let entries = archive.entries().context("Failed to read tar entries")?;
            for entry_result in entries {
                let mut entry = entry_result.context("Failed to read tar entry")?;
                let entry_path = entry.path()?.to_string_lossy().to_string();
                let normalized = entry_path.replace('\\', "/");

                let should_extract = paths.iter().any(|sel| {
                    let sel_norm = sel.replace('\\', "/");
                    normalized == sel_norm || normalized.starts_with(&format!("{}/", sel_norm))
                });

                if should_extract {
                    let out_path = dest.join(&entry_path);
                    if let Some(parent) = out_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if entry.header().entry_type().is_dir() {
                        std::fs::create_dir_all(&out_path)?;
                    } else {
                        entry.unpack(&out_path)?;
                    }
                }
            }
        }
    } else {
        if is_tar_archive(&raw_data) {
            emit_restore_progress(app_handle, key, "extracting", 0.85, "Extracting archive…");
            extract_tar(&raw_data, dest)?;
        } else {
            let output_name = original_filename
                .strip_suffix(".enc")
                .unwrap_or(original_filename);
            let output_path = dest.join(output_name);
            std::fs::write(&output_path, &raw_data).with_context(|| {
                format!("Failed to write restored file to {}", output_path.display())
            })?;
        }
    }

    emit_restore_progress(app_handle, key, "done", 1.0, "Restore complete!");
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Selective restore — only extract specific files from the archive
// ────────────────────────────────────────────────────────────────────

pub async fn run_selective_restore(
    app_handle: tauri::AppHandle,
    api: SaveStateClient,
    master_key: [u8; 32],
    key: String,
    dest: PathBuf,
    original_filename: String,
    selected_paths: Vec<String>,
) {
    let result = run_restore_inner(
        &app_handle,
        api,
        master_key,
        &key,
        &dest,
        &original_filename,
        Some(&selected_paths),
    )
    .await;

    if let Err(e) = result {
        emit_restore_progress(&app_handle, &key, "error", 0.0, &e.to_string());
    }
}

async fn run_selective_restore_inner(
    app_handle: &tauri::AppHandle,
    api: SaveStateClient,
    master_key: [u8; 32],
    key: &str,
    dest: &Path,
    original_filename: &str,
    selected_paths: &[String],
) -> Result<()> {
    // 1. Download
    emit_restore_progress(app_handle, key, "downloading", 0.1, "Getting download URL…");
    let download_resp = api.presign_download(key).await?;

    emit_restore_progress(app_handle, key, "downloading", 0.3, "Downloading…");
    let encrypted = api
        .download_from_presigned_url(&download_resp.download_url)
        .await?;

    // 2. Decrypt
    emit_restore_progress(app_handle, key, "decrypting", 0.5, "Decrypting…");
    let compressed = decrypt_blob(&encrypted, &master_key)?;

    // 3. Decompress
    emit_restore_progress(app_handle, key, "decompressing", 0.7, "Decompressing…");
    let raw_data = decompress_data(&compressed)?;

    // 4. Selective extraction
    if is_tar_archive(&raw_data) {
        emit_restore_progress(
            app_handle,
            key,
            "extracting",
            0.85,
            "Extracting selected files…",
        );

        let mut archive = Archive::new(std::io::Cursor::new(&raw_data));
        let entries = archive.entries().context("Failed to read tar entries")?;

        for entry_result in entries {
            let mut entry = entry_result.context("Failed to read tar entry")?;
            let entry_path = entry.path()?.to_string_lossy().to_string();

            // Normalize separators for matching
            let normalized = entry_path.replace('\\', "/");

            // Check if this entry matches any of the selected paths
            let should_extract = selected_paths.iter().any(|sel| {
                let sel_norm = sel.replace('\\', "/");
                normalized == sel_norm || normalized.starts_with(&format!("{}/", sel_norm))
            });

            if should_extract {
                let out_path = dest.join(&entry_path);
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create dir {:?}", parent))?;
                }

                // Only extract files (skip directories as entries)
                if entry.header().entry_type().is_file() {
                    let mut contents = Vec::new();
                    entry
                        .read_to_end(&mut contents)
                        .with_context(|| format!("Failed to read entry {}", entry_path))?;
                    std::fs::write(&out_path, &contents)
                        .with_context(|| format!("Failed to write {}", out_path.display()))?;
                } else if entry.header().entry_type().is_dir() {
                    std::fs::create_dir_all(&out_path)
                        .with_context(|| format!("Failed to create dir {:?}", out_path))?;
                }
            }
        }
    } else {
        // Single file — just write it
        let output_name = original_filename
            .strip_suffix(".enc")
            .unwrap_or(original_filename);
        let output_path = dest.join(output_name);
        std::fs::write(&output_path, &raw_data).with_context(|| {
            format!("Failed to write restored file to {}", output_path.display())
        })?;
    }

    emit_restore_progress(app_handle, key, "done", 1.0, "Selective restore complete!");
    Ok(())
}

fn emit_restore_progress(
    app: &tauri::AppHandle,
    key: &str,
    stage: &str,
    progress: f64,
    message: &str,
) {
    let payload = RestoreProgress {
        key: key.to_string(),
        stage: stage.to_string(),
        progress,
        message: message.to_string(),
    };
    let _ = app.emit("restore-progress", &payload);
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands
// ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn cmd_restore_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    key: String,
    filename: String,
    destination: String,
) -> std::result::Result<(), String> {
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
    let restore_result =
        crate::kopia::restore_snapshot(&app, state.inner(), &key, &staging.to_string_lossy()).await;

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
