#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod auth;
mod backup;
mod backup_operations;
mod db;
mod incremental;
mod kopia;
mod notifications;
mod profiles;
mod restore;
mod scheduler;
mod state;
mod updates;

use state::{AppState, AppStateWrapper};
use std::sync::Mutex;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager,
};
use tauri_plugin_autostart::MacosLauncher;
#[cfg(target_os = "windows")]
use tauri_plugin_autostart::ManagerExt;

#[cfg(target_os = "windows")]
const WINDOWS_AUTOSTART_INITIALIZED_KEY: &str = "windows_autostart_initialized_v1";

fn launch_requests_minimized(args: &[String]) -> bool {
    args.iter().any(|argument| argument == "--minimized")
}

/// Applies the Windows startup default exactly once.
///
/// The durable marker is written before changing Windows state. This ordering
/// is deliberate: after SaveState has attempted the initial default, a later
/// launch must never re-enable startup behind a user's back. If persisting the
/// marker fails, the Windows startup setting is left untouched.
#[cfg(target_os = "windows")]
fn initialize_windows_autostart<F, E>(
    conn: &rusqlite::Connection,
    enable: F,
) -> anyhow::Result<bool>
where
    F: FnOnce() -> Result<(), E>,
    E: std::fmt::Display,
{
    if db::get_app_metadata(conn, WINDOWS_AUTOSTART_INITIALIZED_KEY)?.is_some() {
        return Ok(false);
    }

    db::set_app_metadata(conn, WINDOWS_AUTOSTART_INITIALIZED_KEY, "attempted")?;
    enable().map_err(|error| anyhow::anyhow!("failed to register Windows autostart: {error}"))?;
    Ok(true)
}

fn main() {
    tauri::Builder::default()
        // ── Plugins ─────────────────────────────────────────────
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--minimized".into()]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if launch_requests_minimized(&args) {
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // ── State ───────────────────────────────────────────────
        .setup(|app| {
            // Autostart should keep schedules alive without flashing the main
            // window during Windows sign-in.
            #[cfg(target_os = "windows")]
            if launch_requests_minimized(&std::env::args().collect::<Vec<_>>()) {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }

            // Determine data directory
            let data_dir = dirs::data_local_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("SaveState");

            // Initialize database
            let conn = db::init_db(&data_dir).expect("Failed to initialize database");
            profiles::migrate_schedule_times_to_local(&conn)
                .expect("Failed to migrate scheduled backup times to machine-local time");

            // Register SaveState as a Windows Startup App once. The durable
            // marker is retained if the user later disables SaveState in
            // Windows Settings or Task Manager, so a normal launch never
            // overrides that Windows-managed choice.
            #[cfg(target_os = "windows")]
            if let Err(error) = initialize_windows_autostart(&conn, || app.autolaunch().enable()) {
                eprintln!("Failed to initialize Windows autostart default: {error}");
            }

            // Build shared state
            let installation_id = db::get_or_create_installation_id(&conn)
                .expect("Failed to initialize installation ID");
            let api_client = api::SaveStateClient::new(installation_id);
            let app_state = AppStateWrapper(Mutex::new(AppState::new(api_client, conn)));

            // Try to restore previous session
            auth::try_restore_session(&app_state);

            // Hand state to Tauri — the scheduler will access it via app handle
            app.manage(app_state);

            // Keep the scheduler alive even when the app starts signed out.
            // A later login can activate schedules without restarting the app.
            let sched_handle = app.handle().clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        run_scheduler_tick(&sched_handle).await;
                    }
                });
            });

            // ── System Tray ─────────────────────────────────────
            let open_item = MenuItemBuilder::with_id("open", "Open SaveState").build(app)?;
            let backup_item = MenuItemBuilder::with_id("backup_now", "Backup Now").build(app)?;
            let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

            let tray_menu = MenuBuilder::new(app)
                .item(&open_item)
                .item(&backup_item)
                .item(&separator)
                .item(&quit_item)
                .build()?;

            let _tray = TrayIconBuilder::new()
                .icon(tauri::include_image!("./icons/32x32.png"))
                .menu(&tray_menu)
                .tooltip("SaveState Vault")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "backup_now" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = window.emit("navigate", "backup");
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            // ── Close → Hide to Tray ────────────────────────────
            if let Some(main_window) = app.get_webview_window("main") {
                // Set the native window icon explicitly so Windows always has
                // an icon for the title bar, Alt+Tab, and taskbar/minimized app.
                main_window.set_icon(tauri::include_image!("./icons/128x128.png"))?;
                let window_clone = main_window.clone();
                main_window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        // Prevent the window from actually closing
                        api.prevent_close();
                        // Hide the window so it goes to the system tray
                        let _ = window_clone.hide();
                    }
                });
            }

            Ok(())
        })
        // ── Commands ────────────────────────────────────────────
        .invoke_handler(tauri::generate_handler![
            // Auth
            auth::cmd_login,
            auth::cmd_confirm_vault_recovery_key,
            auth::cmd_unlock_vault,
            auth::cmd_abandon_vault_unlock,
            auth::cmd_logout,
            auth::cmd_prepare_logout,
            auth::cmd_abort_logout,
            auth::cmd_get_auth_status,
            auth::cmd_get_account,
            auth::cmd_cancel_subscription,
            auth::cmd_resume_subscription,
            // Backup
            backup::cmd_backup_files,
            backup::cmd_backup_folder,
            backup::cmd_delete_backup,
            backup::cmd_get_backup_history,
            // Folders
            backup::cmd_create_folder,
            backup::cmd_move_backup,
            backup::cmd_delete_folder,
            backup::cmd_list_folders,
            // Restore
            restore::cmd_restore_backup,
            restore::cmd_cancel_restore,
            restore::cmd_list_backups,
            restore::cmd_get_backup_manifest,
            restore::cmd_restore_selected_files,
            // Kopia engine (Phase 1: dedup + B2)
            kopia::cmd_warm_repository,
            kopia::cmd_kopia_backup,
            kopia::cmd_kopia_list_snapshots,
            kopia::cmd_kopia_restore,
            kopia::cmd_kopia_set_retention,
            kopia::cmd_kopia_maintenance,
            kopia::cmd_schedule_storage_cleanup,
            // Profiles
            profiles::cmd_create_profile,
            profiles::cmd_update_profile,
            profiles::cmd_delete_profile,
            profiles::cmd_list_profiles,
            profiles::cmd_get_profile_limit,
            profiles::cmd_count_unowned_profiles,
            profiles::cmd_claim_unowned_profiles,
            profiles::cmd_run_profile_backup,
            // Notifications & Settings
            notifications::cmd_save_settings,
            notifications::cmd_get_settings,
            notifications::cmd_test_notification,
            // Updates
            updates::cmd_install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SaveState Vault");
}

#[cfg(all(test, target_os = "windows"))]
mod windows_autostart_tests {
    use super::*;
    use std::cell::Cell;

    fn metadata_connection() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn minimized_autostart_does_not_request_a_visible_window() {
        assert!(launch_requests_minimized(&[
            "savestate-app.exe".into(),
            "--minimized".into()
        ]));
        assert!(!launch_requests_minimized(&["savestate-app.exe".into()]));
    }

    #[test]
    fn enables_once_and_persists_the_attempt_before_returning() {
        let conn = metadata_connection();
        let calls = Cell::new(0);

        assert!(initialize_windows_autostart(&conn, || {
            calls.set(calls.get() + 1);
            Ok::<_, &str>(())
        })
        .unwrap());

        assert_eq!(calls.get(), 1);
        assert_eq!(
            db::get_app_metadata(&conn, WINDOWS_AUTOSTART_INITIALIZED_KEY)
                .unwrap()
                .as_deref(),
            Some("attempted")
        );

        assert!(!initialize_windows_autostart(&conn, || {
            calls.set(calls.get() + 1);
            Ok::<_, &str>(())
        })
        .unwrap());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn an_enable_failure_is_not_retried_on_a_later_launch() {
        let conn = metadata_connection();

        assert!(
            initialize_windows_autostart(&conn, || Err::<(), _>("registry unavailable")).is_err()
        );
        assert_eq!(
            db::get_app_metadata(&conn, WINDOWS_AUTOSTART_INITIALIZED_KEY)
                .unwrap()
                .as_deref(),
            Some("attempted")
        );

        assert!(
            !initialize_windows_autostart(&conn, || -> Result<(), &str> {
                panic!("the initial Windows startup choice must not be overwritten")
            })
            .unwrap()
        );
    }

    #[test]
    fn does_not_touch_windows_when_the_marker_cannot_be_persisted() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let called = Cell::new(false);

        assert!(initialize_windows_autostart(&conn, || {
            called.set(true);
            Ok::<_, &str>(())
        })
        .is_err());
        assert!(!called.get());
    }
}

/// Scheduler tick: check all due profiles and run their backups.
/// This runs on a background thread and accesses state via the app handle.
async fn run_scheduler_tick(app_handle: &tauri::AppHandle) {
    let state: tauri::State<'_, AppStateWrapper> = app_handle.state();

    // Remembered sessions restore both the token and encryption key. Leave
    // schedules pending until both are available.
    let owner_account = state.0.lock().ok().and_then(|guard| guard.account_scope());
    let Some(owner_account) = owner_account else {
        return;
    };

    // Load all profiles
    let profiles = {
        let guard = match state.0.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match db::list_profiles_for_account(&guard.db, &owner_account) {
            Ok(p) => p,
            Err(_) => return,
        }
    };

    profiles::report_schedule_snapshot_from_profiles(&state, &profiles);

    let now = chrono::Utc::now();

    for profile in profiles {
        if !scheduler::profile_is_due(&profile, now) {
            continue;
        }

        // A needs-attention profile receives a fresh, bounded retry budget at
        // its next regular occurrence. Missed occurrences are never replayed;
        // this tick starts exactly one catch-up operation.
        let previous_retry_count = if profile.schedule_state == "needs_attention" {
            if let Ok(guard) = state.0.lock() {
                let _ = db::begin_profile_attempt(&guard.db, &profile.id, &owner_account);
            }
            0
        } else {
            profile.retry_count
        };

        let result = profiles::run_profile_backup_inner(
            app_handle.clone(),
            &state,
            &profile.id,
            "scheduled",
        )
        .await;

        // Send notification based on result
        let api = {
            match state.0.lock() {
                Ok(g) => g.api.clone(),
                Err(_) => continue,
            }
        };

        match result {
            Ok(backup_id) => {
                let recovered = matches!(
                    profile.schedule_state.as_str(),
                    "retrying" | "needs_attention"
                ) || previous_retry_count > 0;
                notifications::send_backup_notification(
                    &api,
                    "backup_success",
                    &profile.name,
                    &if recovered {
                        format!(
                            "Scheduled backup recovered after an automatic retry. ID: {}",
                            backup_id
                        )
                    } else {
                        format!("Scheduled backup completed. ID: {}", backup_id)
                    },
                )
                .await;
            }
            Err(e) => {
                let error = e.to_string();
                if error.contains("BACKUP_CANCELLED") {
                    // Signing out is not a failed occurrence. Skip retries and
                    // advance to the next normal cadence without changing the
                    // last successful run timestamp.
                    let next_regular = profiles::compute_next_run(profile.schedule.as_deref());
                    if let Ok(guard) = state.0.lock() {
                        let _ = db::advance_profile_after_cancellation(
                            &guard.db,
                            &profile.id,
                            &owner_account,
                            next_regular.as_deref(),
                        );
                    }
                    profiles::report_schedule_snapshot(&state);
                    continue;
                }
                let classification = scheduler::classify_schedule_failure(&error);
                let bounded_error: String = error.chars().take(1_000).collect();
                let next_retry_number = previous_retry_count + 1;
                let retry_delay = classification
                    .retryable
                    .then(|| scheduler::retry_delay(&profile.id, next_retry_number))
                    .flatten();

                if let Some(delay) = retry_delay {
                    let retry_at = (chrono::Utc::now() + delay).to_rfc3339();
                    if let Ok(guard) = state.0.lock() {
                        let _ = db::schedule_profile_retry(
                            &guard.db,
                            &profile.id,
                            &owner_account,
                            next_retry_number,
                            &retry_at,
                            classification.code,
                            &bounded_error,
                        );
                    }
                    // Notify on the first failure only; intermediate retries
                    // stay quiet and a later success produces recovery notice.
                    if previous_retry_count == 0 {
                        notifications::send_backup_notification(
                            &api,
                            "backup_failure",
                            &profile.name,
                            &format!(
                                "Scheduled backup failed ({}). Automatic retry {} of {} is queued.",
                                classification.code,
                                next_retry_number,
                                scheduler::MAX_SCHEDULE_RETRIES,
                            ),
                        )
                        .await;
                    }
                } else {
                    let next_regular = profiles::compute_next_run(profile.schedule.as_deref());
                    if let Ok(guard) = state.0.lock() {
                        let _ = db::mark_profile_needs_attention(
                            &guard.db,
                            &profile.id,
                            &owner_account,
                            next_regular.as_deref(),
                            previous_retry_count,
                            classification.code,
                            &bounded_error,
                        );
                    }
                    notifications::send_backup_notification(
                        &api,
                        "backup_failure",
                        &profile.name,
                        &if classification.retryable {
                            format!(
                                "Scheduled backup still failed after {} automatic retries ({}). It needs attention and will try again at the next regular time.",
                                scheduler::MAX_SCHEDULE_RETRIES,
                                classification.code,
                            )
                        } else {
                            format!(
                                "Scheduled backup needs attention ({}). Automatic retries were stopped because user action is required.",
                                classification.code,
                            )
                        },
                    )
                    .await;
                }
            }
        }

        // Report the new future run, retry deadline, or needs-attention state
        // immediately instead of waiting for the next minute tick.
        profiles::report_schedule_snapshot(&state);
    }
}
