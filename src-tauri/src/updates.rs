use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProgress {
    stage: &'static str,
    version: String,
    downloaded: u64,
    total: Option<u64>,
}

/// Download and install the latest update only when the backup engine is idle.
/// On Windows the updater starts the installer and exits this process after the
/// signed package has downloaded successfully.
#[tauri::command]
pub async fn cmd_install_update(app: AppHandle) -> Result<(), String> {
    let _update_guard = crate::kopia::try_begin_update()
        .map_err(|error| format!("UPDATE_BUSY: {}. The update will not interrupt it.", error))?;

    let update = app
        .updater()
        .map_err(|error| format!("Could not initialize updater: {}", error))?
        .check()
        .await
        .map_err(|error| format!("Could not check for updates: {}", error))?
        .ok_or_else(|| "No newer version is available".to_string())?;

    let version = update.version.clone();
    let _ = app.emit(
        "update-progress",
        UpdateProgress {
            stage: "started",
            version: version.clone(),
            downloaded: 0,
            total: None,
        },
    );

    let progress_app = app.clone();
    let progress_version = version.clone();
    let mut downloaded = 0_u64;
    update
        .download_and_install(
            move |chunk_length, content_length| {
                downloaded = downloaded.saturating_add(chunk_length as u64);
                let _ = progress_app.emit(
                    "update-progress",
                    UpdateProgress {
                        stage: "progress",
                        version: progress_version.clone(),
                        downloaded,
                        total: content_length,
                    },
                );
            },
            {
                let finished_app = app.clone();
                let finished_version = version.clone();
                move || {
                    let _ = finished_app.emit(
                        "update-progress",
                        UpdateProgress {
                            stage: "downloaded",
                            version: finished_version,
                            downloaded: 0,
                            total: None,
                        },
                    );
                }
            },
        )
        .await
        .map_err(|error| format!("Update failed: {}", error))?;

    // Windows exits from the installer call above. Other desktop targets can
    // relaunch from the frontend after this command returns.
    Ok(())
}
