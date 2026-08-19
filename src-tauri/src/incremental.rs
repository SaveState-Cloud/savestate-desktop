use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// ────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────

/// A single file discovered during a directory scan.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// Path relative to the scanned root directory.
    pub relative_path: String,
    /// Absolute path on disk.
    pub absolute_path: PathBuf,
    /// SHA-256 hex digest of file contents.
    pub hash: String,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified timestamp (RFC 3339).
    pub modified: String,
}

/// The result of diffing scanned files against a previous snapshot.
#[derive(Debug, Clone)]
pub struct ChangeSet {
    /// Files whose hash has changed since the last snapshot.
    pub changed: Vec<ScannedFile>,
    /// Files that are new (not in the previous snapshot).
    pub added: Vec<ScannedFile>,
    /// Relative paths of files that were in the snapshot but no longer on disk.
    pub deleted: Vec<String>,
    /// Files that are identical to the previous snapshot.
    pub unchanged: Vec<ScannedFile>,
    /// True if no prior snapshot exists (everything is new).
    pub is_full: bool,
}

// ────────────────────────────────────────────────────────────────────
// Directory scanning
// ────────────────────────────────────────────────────────────────────

/// Walk `path` recursively, computing SHA-256 hashes for every regular file.
pub fn scan_directory(path: &Path) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.context("Failed to read directory entry")?;

        // Skip directories — we only care about files
        if !entry.file_type().is_file() {
            continue;
        }

        let abs = entry.path().to_path_buf();

        // Compute relative path from the root
        let rel = abs
            .strip_prefix(path)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");

        // Read the file and compute SHA-256
        let data = std::fs::read(&abs)
            .with_context(|| format!("Failed to read file: {}", abs.display()))?;
        let hash = hex::encode(Sha256::digest(&data));

        // Get metadata
        let meta = entry.metadata().context("Failed to read metadata")?;
        let size = meta.len();
        let modified = meta
            .modified()
            .map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.to_rfc3339()
            })
            .unwrap_or_default();

        files.push(ScannedFile {
            relative_path: rel,
            absolute_path: abs,
            hash,
            size,
            modified,
        });
    }

    Ok(files)
}

// ────────────────────────────────────────────────────────────────────
// Diff against snapshot
// ────────────────────────────────────────────────────────────────────

/// Compare scanned files against a set of prior snapshots to produce a ChangeSet.
pub fn diff_against_snapshot(
    scanned_files: &[ScannedFile],
    snapshots: &[crate::db::FileSnapshot],
) -> ChangeSet {
    use std::collections::HashMap;

    // No previous snapshots → everything is new (full backup)
    if snapshots.is_empty() {
        return ChangeSet {
            changed: Vec::new(),
            added: scanned_files.to_vec(),
            deleted: Vec::new(),
            unchanged: Vec::new(),
            is_full: true,
        };
    }

    // Build a lookup from relative path → old hash
    let snap_map: HashMap<&str, &str> = snapshots
        .iter()
        .map(|s| (s.file_path.as_str(), s.file_hash.as_str()))
        .collect();

    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut unchanged = Vec::new();

    // Track which snapshot entries we've seen
    let mut seen_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for file in scanned_files {
        seen_paths.insert(&file.relative_path);
        match snap_map.get(file.relative_path.as_str()) {
            Some(&old_hash) => {
                if old_hash != file.hash {
                    changed.push(file.clone());
                } else {
                    unchanged.push(file.clone());
                }
            }
            None => {
                added.push(file.clone());
            }
        }
    }

    // Deleted = in snapshot but not on disk
    let deleted: Vec<String> = snapshots
        .iter()
        .filter(|s| !seen_paths.contains(s.file_path.as_str()))
        .map(|s| s.file_path.clone())
        .collect();

    ChangeSet {
        changed,
        added,
        deleted,
        unchanged,
        is_full: false,
    }
}

// ────────────────────────────────────────────────────────────────────
// Manifest builder
// ────────────────────────────────────────────────────────────────────

/// Build a JSON manifest describing the backup contents.
pub fn build_manifest(
    profile_id: &str,
    backup_id: &str,
    all_files: &[ScannedFile],
    change_type: &str,
) -> serde_json::Value {
    let file_list: Vec<serde_json::Value> = all_files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.relative_path,
                "hash": f.hash,
                "size": f.size,
                "modified": f.modified,
            })
        })
        .collect();

    serde_json::json!({
        "profileId": profile_id,
        "backupId": backup_id,
        "changeType": change_type,
        "fileCount": file_list.len(),
        "files": file_list,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}
