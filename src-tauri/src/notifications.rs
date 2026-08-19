use crate::api::SaveStateClient;
use crate::state::AppStateWrapper;

// ────────────────────────────────────────────────────────────────────
// Helper — send a notification through the API (best-effort)
// ────────────────────────────────────────────────────────────────────

/// Fire-and-forget notification delivery.
/// Called by the scheduler and other internal processes.
pub async fn send_backup_notification(
    api: &SaveStateClient,
    event_type: &str,
    profile_name: &str,
    details: &str,
) {
    let failed = event_type.contains("failure");
    let payload = serde_json::json!({
        "type": event_type,
        "profileName": profile_name,
        "details": details,
        "status": if failed { "failed" } else { "completed" },
    });

    // Notification delivery must never make a completed backup fail, but it
    // also must not disappear silently. The API response is intentionally
    // bounded and contains no webhook secret.
    match api.send_notification(&payload).await {
        Ok(result) if result.get("sent").and_then(|value| value.as_bool()) == Some(true) => {}
        Ok(result) => {
            let reason = result
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("notification destination rejected delivery");
            eprintln!("SaveState notification was not delivered: {}", reason);
        }
        Err(error) => eprintln!("SaveState notification request failed: {}", error),
    }
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands
// ────────────────────────────────────────────────────────────────────

/// Save user notification / webhook settings to the server.
#[tauri::command]
pub async fn cmd_save_settings(
    state: tauri::State<'_, AppStateWrapper>,
    settings: crate::api::UserSettings,
) -> std::result::Result<(), String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.save_settings(&settings)
        .await
        .map_err(|e| e.to_string())
}

/// Retrieve user notification / webhook settings from the server.
#[tauri::command]
pub async fn cmd_get_settings(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<crate::api::UserSettings, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };
    api.get_settings().await.map_err(|e| e.to_string())
}

/// Send a test notification to verify the user's webhook is configured correctly.
/// Returns the API response so the frontend can display results.
#[tauri::command]
pub async fn cmd_test_notification(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock: {}", e))?;
        guard.api.clone()
    };

    let payload = serde_json::json!({
        "type": "test",
        "profileName": "Test",
        "details": "This is a test notification from SaveState Vault.",
        "status": "test",
    });

    let result = api
        .send_notification(&payload)
        .await
        .map_err(|e| e.to_string())?;

    Ok(result)
}
