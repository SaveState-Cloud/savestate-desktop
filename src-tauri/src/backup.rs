use crate::db;
use crate::state::AppStateWrapper;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tar::Builder as TarBuilder;
use tauri::Emitter;

// ────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────

const NONCE_LEN: usize = 12;

// ────────────────────────────────────────────────────────────────────
// Progress event payload
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct BackupProgress {
    pub id: String,
    pub stage: String, // "compressing", "encrypting", "uploading", "done", "error"
    pub progress: f64, // 0.0 – 1.0
    pub message: String,
}

// ────────────────────────────────────────────────────────────────────
// Crypto helpers — uses master_key directly (no Argon2 derivation)
// ────────────────────────────────────────────────────────────────────

/// Encrypt data with AES-256-GCM using the master key directly.
/// Output format: `[12-byte nonce][ciphertext + tag]`
pub fn encrypt_blob(data: &[u8], master_key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let cipher =
        Aes256Gcm::new_from_slice(master_key).map_err(|e| anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|e| anyhow!("Encryption failed: {}", e))?;

    let mut output = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

// ── V2 Streaming Encryption ─────────────────────────────────────────

pub const V2_MAGIC: &[u8; 12] = b"SAVESTATE_V2";
pub const CHUNK_SIZE_PLAINTEXT: usize = 25 * 1024 * 1024; // 25 MB

/// Encrypt a chunk for V2 streaming format.
pub fn encrypt_chunk_v2(
    data: &[u8],
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
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|e| anyhow!("Encryption failed: {}", e))?;

    let mut output = Vec::new();
    if chunk_index == 1 {
        output.extend_from_slice(V2_MAGIC);
        output.extend_from_slice(base_nonce);
    }
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// A custom writer that buffers data up to `CHUNK_SIZE_PLAINTEXT` and sends it to a channel.
pub struct ChunkSender {
    pub tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    pub buffer: Vec<u8>,
}

impl std::io::Write for ChunkSender {
    fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
        let original_len = buf.len();
        while !buf.is_empty() {
            let space = CHUNK_SIZE_PLAINTEXT - self.buffer.len();
            let take = std::cmp::min(space, buf.len());
            self.buffer.extend_from_slice(&buf[..take]);
            buf = &buf[take..];

            if self.buffer.len() >= CHUNK_SIZE_PLAINTEXT {
                let chunk =
                    std::mem::replace(&mut self.buffer, Vec::with_capacity(CHUNK_SIZE_PLAINTEXT));
                self.tx
                    .blocking_send(chunk)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            }
        }
        Ok(original_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            let chunk = std::mem::replace(&mut self.buffer, Vec::new());
            self.tx
                .blocking_send(chunk)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────
// Compression helpers
// ────────────────────────────────────────────────────────────────────

fn compress_data(data: &[u8]) -> Result<Vec<u8>> {
    let compressed =
        zstd::encode_all(std::io::Cursor::new(data), 3).context("Zstd compression failed")?;
    Ok(compressed)
}

// ────────────────────────────────────────────────────────────────────
// Tar helper
// ────────────────────────────────────────────────────────────────────

fn tar_directory(dir_path: &Path) -> Result<Vec<u8>> {
    let mut archive_buf = Vec::new();
    {
        let mut builder = TarBuilder::new(&mut archive_buf);
        let dir_name = dir_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        builder
            .append_dir_all(&dir_name, dir_path)
            .context("Failed to tar directory")?;
        builder.finish().context("Failed to finish tar")?;
    }
    Ok(archive_buf)
}

// ────────────────────────────────────────────────────────────────────
// Core backup pipeline
// ────────────────────────────────────────────────────────────────────

async fn run_backup(
    app_handle: tauri::AppHandle,
    state: &AppStateWrapper,
    source: BackupSource,
) -> Result<String> {
    use rand::RngCore;

    let backup_id = uuid::Uuid::new_v4().to_string();

    // 1. Determine display name and original size
    let (display_name, original_size) = match &source {
        BackupSource::Files(paths) => {
            if paths.len() == 1 {
                let p = &paths[0];
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as i64;
                (name, size)
            } else {
                let name = format!("multi-{}.tar", &backup_id[..8]);
                let size: u64 = paths
                    .iter()
                    .filter_map(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum();
                (name, size as i64)
            }
        }
        BackupSource::Directory(dir) => {
            let name = format!(
                "{}.tar",
                dir.file_name().unwrap_or_default().to_string_lossy()
            );
            let size = std::fs::metadata(dir).map(|m| m.len()).unwrap_or(0) as i64;
            (name, size)
        }
    };

    let timestamp = chrono::Utc::now().format("%Y-%m-%d_%H%M%S").to_string();
    let upload_filename = format!("{}_{}.stream.enc", display_name, timestamp);

    // 2. Get API client and master key
    let (api, master_key) = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        let key = guard
            .master_key
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        (guard.api.clone(), key)
    };

    emit_progress(
        &app_handle,
        &backup_id,
        "compressing",
        0.1,
        "Initializing backup…",
    );

    // 3. Create multipart upload
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

    // Record start in DB
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

    // 4. Stream: tar → compress → chunk → channel
    let (tx, mut rx) = tokio::sync::mpsc::channel(6);
    let source_clone = match &source {
        BackupSource::Files(paths) => paths.clone(),
        BackupSource::Directory(dir) => vec![dir.clone()],
    };
    let is_single_file = matches!(&source, BackupSource::Files(p) if p.len() == 1);
    let is_directory = matches!(&source, BackupSource::Directory(_));
    let dir_name = display_name.trim_end_matches(".tar").to_string();

    let stream_task = tokio::task::spawn_blocking(move || -> Result<()> {
        let sender = ChunkSender {
            tx,
            buffer: Vec::with_capacity(CHUNK_SIZE_PLAINTEXT),
        };
        let mut encoder = zstd::Encoder::new(sender, 3)?;
        {
            let mut builder = TarBuilder::new(&mut encoder);
            if is_directory {
                for entry in walkdir::WalkDir::new(&source_clone[0])
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let path = entry.path();
                    if path.is_file() {
                        let rel_path = path.strip_prefix(&source_clone[0]).unwrap_or(path);
                        if let Ok(mut file) = std::fs::File::open(path) {
                            let name = std::path::Path::new(&dir_name).join(rel_path);
                            let name_str = name.to_string_lossy().replace('\\', "/");
                            let _ = builder.append_file(&name_str, &mut file);
                        }
                    }
                }
            } else if is_single_file {
                let p = &source_clone[0];
                let mut file = std::fs::File::open(p)
                    .with_context(|| format!("Failed to open {}", p.display()))?;
                let fname = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                builder
                    .append_file(&fname, &mut file)
                    .with_context(|| format!("Failed to add {} to tar", p.display()))?;
            } else {
                for p in &source_clone {
                    let fname = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let mut file = std::fs::File::open(p)
                        .with_context(|| format!("Failed to open {}", p.display()))?;
                    builder
                        .append_file(&fname, &mut file)
                        .with_context(|| format!("Failed to add {} to tar", p.display()))?;
                }
            }
            builder.finish()?;
        }
        let mut sender = encoder.finish()?;
        use std::io::Write;
        sender.flush()?;
        Ok(())
    });

    // 5. Encrypt chunks and upload concurrently
    let mut base_nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut base_nonce);

    let mut part_number: u32 = 1;
    let mut total_encrypted_size: i64 = 0;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(6));
    let mut upload_tasks = Vec::new();

    while let Some(chunk) = rx.recv().await {
        let encrypted = encrypt_chunk_v2(&chunk, &base_nonce, part_number, &master_key)?;
        total_encrypted_size += encrypted.len() as i64;

        emit_progress(
            &app_handle,
            &backup_id,
            "uploading",
            0.1 + 0.7 * (part_number as f64 / (part_number as f64 + 2.0)),
            &format!("Uploading part {}…", part_number),
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
                    CHUNK_SIZE_PLAINTEXT as u64,
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

    // 6. Wait for streaming to finish
    if let Err(e) = stream_task
        .await
        .map_err(|e| anyhow!("Stream task panicked: {}", e))?
    {
        let guard = state.0.lock().map_err(|e2| anyhow!("Lock: {}", e2))?;
        let _ = db::record_backup_failed(&guard.db, &backup_id, &e.to_string());
        return Err(e);
    }

    // 7. Collect upload results
    let mut parts = Vec::new();
    for task in upload_tasks {
        let part = task.await.map_err(|e| anyhow!("Task panicked: {}", e))??;
        parts.push(part);
    }
    parts.sort_by_key(|p| p.part_number);

    // 8. Complete multipart upload
    api.multipart_complete(&upload_key, &upload_id, parts)
        .await?;

    // 9. Record complete in DB
    {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock: {}", e))?;
        db::record_backup_complete(&guard.db, &backup_id, total_encrypted_size)?;
    }

    // 10. Build and upload manifest
    emit_progress(&app_handle, &backup_id, "manifest", 0.9, "Saving manifest…");

    let manifest = build_backup_manifest(&source);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    let manifest_key = format!("{}_manifest.json", upload_key.trim_end_matches(".enc"));

    let manifest_result: Result<()> = async {
        let manifest_presign = api
            .presign_manifest(&manifest_key, "application/json")
            .await?;
        api.upload_to_presigned_url(
            &manifest_presign.upload_url,
            manifest_bytes,
            "application/json",
            manifest_presign.expires_in,
        )
        .await?;
        Ok(())
    }
    .await;

    if let Err(e) = manifest_result {
        eprintln!("Warning: manifest upload failed: {}", e);
    }

    // 11. Done!
    emit_progress(&app_handle, &backup_id, "done", 1.0, "Backup complete!");
    crate::notifications::send_backup_notification(
        &api,
        "backup_success",
        "Quick Backup",
        &format!("Manual backup completed. ID: {}", backup_id),
    )
    .await;
    Ok(backup_id)
}

enum BackupSource {
    Files(Vec<PathBuf>),
    Directory(PathBuf),
}

fn emit_progress(app: &tauri::AppHandle, id: &str, stage: &str, progress: f64, message: &str) {
    let payload = BackupProgress {
        id: id.to_string(),
        stage: stage.to_string(),
        progress,
        message: message.to_string(),
    };
    let _ = app.emit("backup-progress", &payload);
}

/// Build a manifest JSON for the given backup source.
fn build_backup_manifest(source: &BackupSource) -> serde_json::Value {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let mut files = Vec::new();

    match source {
        BackupSource::Directory(dir) => {
            if let Ok(entries) = walkdir::WalkDir::new(dir)
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                for entry in entries {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let rel = entry
                        .path()
                        .strip_prefix(dir)
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
        }
        BackupSource::Files(paths) => {
            for p in paths {
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let modified = std::fs::metadata(p)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default();
                files.push(serde_json::json!({
                    "path": name,
                    "size": size,
                    "modified": modified,
                }));
            }
        }
    }

    serde_json::json!({
        "source": match source {
            BackupSource::Directory(d) => d.to_string_lossy().to_string(),
            BackupSource::Files(ps) => format!("{} file(s)", ps.len()),
        },
        "timestamp": timestamp,
        "files": files,
    })
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands
// ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn cmd_backup_files(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    paths: Vec<String>,
    folder: Option<String>,
) -> std::result::Result<String, String> {
    if paths.is_empty() {
        return Err("No files selected".to_string());
    }
    let path_bufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    // Validate all paths exist
    for p in &path_bufs {
        if !p.exists() {
            return Err(format!("File not found: {}", p.display()));
        }
    }
    let display_name = if path_bufs.len() == 1 {
        path_bufs[0]
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("Selected file")
            .to_string()
    } else {
        format!("{} selected files", path_bufs.len())
    };
    let api = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {}", error))?;
        guard.api.clone()
    };
    let paths: Vec<String> = path_bufs
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let result = crate::kopia::backup_paths_in_folder(
        &app,
        state.inner(),
        paths,
        folder.as_deref().unwrap_or("/"),
    )
    .await;
    match &result {
        Ok(backup_id) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_success",
                &display_name,
                &format!("Quick backup completed. ID: {}", backup_id),
            )
            .await
        }
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_failure",
                &display_name,
                "Quick backup failed. Open SaveState Vault for details.",
            )
            .await
        }
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_backup_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    path: String,
    folder: Option<String>,
) -> std::result::Result<String, String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }
    let display_name = dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Selected folder")
        .to_string();
    let api = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {}", error))?;
        guard.api.clone()
    };
    let result = crate::kopia::backup_paths_in_folder(
        &app,
        state.inner(),
        vec![path],
        folder.as_deref().unwrap_or("/"),
    )
    .await;
    match &result {
        Ok(backup_id) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_success",
                &display_name,
                &format!("Quick backup completed. ID: {}", backup_id),
            )
            .await
        }
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_failure",
                &display_name,
                "Quick backup failed. Open SaveState Vault for details.",
            )
            .await
        }
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_delete_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    key: String,
) -> std::result::Result<(), String> {
    crate::kopia::delete_snapshot(&app, state.inner(), &key)
        .await
        .map_err(|e| e.to_string())?;

    // Delete local record by remote_key
    {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        let _ = guard.db.execute(
            "DELETE FROM backup_history WHERE remote_key = ?1",
            rusqlite::params![key],
        );
    }

    // Reclaim unreachable Kopia packs in the background. The backup disappears
    // immediately, while the dashboard honestly reports cleanup as pending.
    crate::kopia::schedule_storage_cleanup(app.clone());

    Ok(())
}

#[tauri::command]
pub async fn cmd_get_backup_history(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<db::BackupRecord>, String> {
    let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
    db::get_backup_history(&guard.db).map_err(|e| e.to_string())
}

// ────────────────────────────────────────────────────────────────────
// Folder operations
// ────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn cmd_create_folder(
    state: tauri::State<'_, AppStateWrapper>,
    name: String,
    parent_folder: Option<String>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.create_folder(&name, parent_folder.as_deref().unwrap_or("/"))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_move_backup(
    state: tauri::State<'_, AppStateWrapper>,
    key: String,
    destination_folder: String,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.move_backup(&key, &destination_folder)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_delete_folder(
    state: tauri::State<'_, AppStateWrapper>,
    name: String,
) -> std::result::Result<(), String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.delete_folder(&name).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_list_folders(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.list_folders().await.map_err(|e| e.to_string())
}
