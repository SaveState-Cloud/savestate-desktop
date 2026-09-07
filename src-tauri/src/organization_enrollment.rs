use crate::api::{
    OrganizationAvailableInstallationsResponse, OrganizationBackupHeartbeat,
    OrganizationEnrollmentPreviewResponse, OrganizationEnrollmentRedeemResponse, SaveStateClient,
};
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

const ORGANIZATION_INSTALLATION_METADATA_KEY: &str = "organization_installation_id";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredOrganizationInstallation {
    account_email: String,
    installation_id: String,
    server_label: String,
    connected_at: String,
    device_credential: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    pending_backup: Option<OrganizationBackupHeartbeat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationInstallationStatus {
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationInstallationConnection {
    pub connected: bool,
    pub installation_id: String,
    pub server_label: String,
    pub connected_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persistence_warning: Option<String>,
}

fn credential_entry() -> Result<keyring::v1::Entry> {
    keyring::v1::Entry::new(
        &crate::runtime_storage::credential_service("SaveState Vault"),
        "organization-installation",
    )
    .context("Windows Credential Manager is unavailable")
}

trait InstallationBackend {
    fn load(&self) -> Result<Option<StoredOrganizationInstallation>>;
    fn save(&mut self, value: &StoredOrganizationInstallation) -> Result<()>;
    fn remove(&mut self) -> Result<()>;
}

struct CredentialBackend;

impl InstallationBackend for CredentialBackend {
    fn load(&self) -> Result<Option<StoredOrganizationInstallation>> {
        match credential_entry()?.get_secret() {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("Failed to read the organization device credential"),
        }
    }

    fn save(&mut self, value: &StoredOrganizationInstallation) -> Result<()> {
        let data = serde_json::to_vec(value)?;
        credential_entry()?
            .set_secret(&data)
            .context("Failed to save the organization device credential securely")
    }

    fn remove(&mut self) -> Result<()> {
        credential_entry()?
            .delete_credential()
            .context("Failed to remove the disabled organization device credential")
    }
}

// All read/modify/write operations share one lock. Never hold it during HTTP I/O.
struct InstallationStore<B>(Mutex<B>);
static INSTALLATIONS: InstallationStore<CredentialBackend> =
    InstallationStore(Mutex::new(CredentialBackend));

impl<B: InstallationBackend> InstallationStore<B> {
    fn load(&self) -> Result<Option<StoredOrganizationInstallation>> {
        self.0
            .lock()
            .map_err(|_| anyhow!("Organization credential lock failed"))?
            .load()
    }

    fn save(&self, value: &StoredOrganizationInstallation) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow!("Organization credential lock failed"))?
            .save(value)
    }

    fn update(
        &self,
        change: impl FnOnce(&mut StoredOrganizationInstallation) -> bool,
    ) -> Result<()> {
        let mut backend = self
            .0
            .lock()
            .map_err(|_| anyhow!("Organization credential lock failed"))?;
        if let Some(mut current) = backend.load()? {
            if change(&mut current) {
                backend.save(&current)?;
            }
        }
        Ok(())
    }

    fn remove_if_current(&self, sent: &StoredOrganizationInstallation) -> Result<()> {
        let mut backend = self
            .0
            .lock()
            .map_err(|_| anyhow!("Organization credential lock failed"))?;
        if backend
            .load()?
            .as_ref()
            .is_some_and(|current| current.same_binding(sent))
        {
            backend.remove()?;
        }
        Ok(())
    }

    fn acknowledge(
        &self,
        sent: &StoredOrganizationInstallation,
        event_id: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        self.update(|current| {
            if !current.same_binding(sent) {
                return false;
            }
            let mut changed = false;
            if current.workspace_id.is_none() {
                if let Some(workspace_id) = workspace_id {
                    current.workspace_id = Some(workspace_id.to_owned());
                    changed = true;
                }
            }
            if event_id.is_some()
                && current
                    .pending_backup
                    .as_ref()
                    .map(|event| event.event_id.as_str())
                    == event_id
            {
                current.pending_backup = None;
                changed = true;
            }
            changed
        })
    }

    fn queue(
        &self,
        account_scope: &str,
        backup: &OrganizationBackupHeartbeat,
    ) -> Result<Option<StoredOrganizationInstallation>> {
        let mut queued = None;
        self.update(|current| {
            if !current.matches_scope(account_scope) {
                return false;
            }
            current.pending_backup = Some(backup.clone());
            queued = Some(current.clone());
            true
        })?;
        Ok(queued)
    }
}

impl StoredOrganizationInstallation {
    fn same_binding(&self, other: &Self) -> bool {
        self.account_email
            .eq_ignore_ascii_case(&other.account_email)
            && self.installation_id == other.installation_id
            && self.device_credential == other.device_credential
    }

    fn matches_scope(&self, scope: &str) -> bool {
        self.workspace_id.as_ref().is_some_and(|workspace| {
            scope.eq_ignore_ascii_case(&format!("{}::{workspace}", self.account_email.trim()))
        })
    }
}

fn device_credential_was_revoked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("invalid_device_credential"))
}

fn installation_for_account(account_email: &str) -> Option<StoredOrganizationInstallation> {
    let stored = INSTALLATIONS.load().ok()??;
    stored
        .account_email
        .trim()
        .eq_ignore_ascii_case(account_email.trim())
        .then_some(stored)
}

fn status_for_account(
    stored: Option<&StoredOrganizationInstallation>,
    account_email: Option<&str>,
) -> OrganizationInstallationStatus {
    let Some(stored) = stored else {
        return OrganizationInstallationStatus {
            connected: false,
            installation_id: None,
            server_label: None,
            connected_at: None,
        };
    };
    let account_matches = account_email.is_some_and(|email| {
        stored
            .account_email
            .trim()
            .eq_ignore_ascii_case(email.trim())
    });
    if !account_matches {
        return OrganizationInstallationStatus {
            connected: false,
            installation_id: None,
            server_label: None,
            connected_at: None,
        };
    }
    OrganizationInstallationStatus {
        connected: true,
        installation_id: Some(stored.installation_id.clone()),
        server_label: Some(stored.server_label.clone()),
        connected_at: Some(stored.connected_at.clone()),
    }
}

pub(crate) fn queue_organization_installation_backup_heartbeat(
    api: &SaveStateClient,
    account_scope: &str,
    backup: OrganizationBackupHeartbeat,
) {
    let stored = match INSTALLATIONS.queue(account_scope, &backup) {
        Ok(Some(stored)) => stored,
        Ok(None) => return,
        Err(error) => {
            eprintln!("Failed to persist organization backup health for retry: {error}");
            return;
        }
    };
    let api = api.clone();
    tokio::spawn(async move {
        let event_id = backup.event_id.clone();
        let retry_delays = [0, 2, 10];
        for delay_seconds in retry_delays {
            if delay_seconds > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(delay_seconds)).await;
            }
            match api
                .organization_installation_heartbeat(
                    &stored.device_credential,
                    Some(backup.clone()),
                )
                .await
            {
                Ok(response) => {
                    if let Err(error) =
                        INSTALLATIONS.acknowledge(&stored, Some(&event_id), response.workspace_id())
                    {
                        eprintln!("Failed to acknowledge organization backup health: {error}");
                    }
                    return;
                }
                Err(error) => {
                    if device_credential_was_revoked(&error) {
                        if let Err(remove_error) = INSTALLATIONS.remove_if_current(&stored) {
                            eprintln!(
                                "Failed to remove revoked organization credential: {remove_error}"
                            );
                        }
                        return;
                    }
                    eprintln!("Organization installation health report failed: {error}");
                }
            }
        }
    });
}

pub(crate) async fn send_organization_installation_heartbeat(
    state: &AppStateWrapper,
) -> Result<()> {
    let (api, account_email) = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        let Some(account_email) = guard.account_email() else {
            return Ok(());
        };
        (guard.api.clone(), account_email)
    };
    let Some(stored) = installation_for_account(&account_email) else {
        return Ok(());
    };
    let pending_backup = stored.pending_backup.clone();
    let response = match api
        .organization_installation_heartbeat(&stored.device_credential, pending_backup.clone())
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if device_credential_was_revoked(&error) {
                INSTALLATIONS.remove_if_current(&stored)?;
                return Ok(());
            }
            return Err(error);
        }
    };
    INSTALLATIONS.acknowledge(
        &stored,
        pending_backup.as_ref().map(|b| b.event_id.as_str()),
        response.workspace_id(),
    )?;
    Ok(())
}

#[tauri::command]
pub async fn cmd_get_organization_installation_status(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<OrganizationInstallationStatus, String> {
    let account_email = state
        .0
        .lock()
        .map_err(|error| format!("Lock error: {error}"))?
        .account_email();
    Ok(status_for_account(
        INSTALLATIONS
            .load()
            .map_err(|error| error.to_string())?
            .as_ref(),
        account_email.as_deref(),
    ))
}

#[tauri::command]
pub async fn cmd_inspect_organization_installation(
    state: tauri::State<'_, AppStateWrapper>,
    token: String,
) -> std::result::Result<OrganizationEnrollmentPreviewResponse, String> {
    let api = {
        let guard = state
            .0
            .lock()
            .map_err(|error| format!("Lock error: {error}"))?;
        if guard.account_email().is_none() {
            return Err(
                "Sign in and unlock this vault before connecting an organization installation"
                    .into(),
            );
        }
        guard.api.clone()
    };
    api.inspect_organization_installation(token.trim())
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_list_available_organization_installations(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<OrganizationAvailableInstallationsResponse, String> {
    let api = {
        let guard = state
            .0
            .lock()
            .map_err(|error| format!("Lock error: {error}"))?;
        if guard.account_email().is_none() {
            return Err("Sign in and unlock this vault to view organization storage".into());
        }
        guard.api.clone()
    };
    api.available_organization_installations()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_connect_organization_installation(
    state: tauri::State<'_, AppStateWrapper>,
    installation_id: String,
) -> std::result::Result<OrganizationInstallationConnection, String> {
    connect_organization_installation(state.inner(), installation_id.trim())
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_redeem_organization_installation(
    state: tauri::State<'_, AppStateWrapper>,
    token: String,
) -> std::result::Result<OrganizationInstallationConnection, String> {
    redeem_organization_installation(state.inner(), token.trim())
        .await
        .map_err(|error| error.to_string())
}

async fn redeem_organization_installation(
    state: &AppStateWrapper,
    token: &str,
) -> Result<OrganizationInstallationConnection> {
    // Reserve before the API call: a busy engine must not consume a one-use token.
    // Keep the guard through credential persistence and the service-session switch.
    let _session_change = crate::backup_operations::begin_session_change()?;
    let (api, session_generation, account_email, master_key) = connection_context(state)?;
    let response = api.redeem_organization_installation(token).await?;
    finish_organization_installation_connection(
        state,
        response,
        session_generation,
        account_email,
        master_key,
    )
}

async fn connect_organization_installation(
    state: &AppStateWrapper,
    installation_id: &str,
) -> Result<OrganizationInstallationConnection> {
    // Account-first connection changes the repository just like token redemption.
    let _session_change = crate::backup_operations::begin_session_change()?;
    let (api, session_generation, account_email, master_key) = connection_context(state)?;
    let response = api
        .connect_organization_installation(installation_id)
        .await?;
    finish_organization_installation_connection(
        state,
        response,
        session_generation,
        account_email,
        master_key,
    )
}

fn connection_context(state: &AppStateWrapper) -> Result<(SaveStateClient, u64, String, [u8; 32])> {
    {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        let account_email = guard.account_email().ok_or_else(|| {
            anyhow!("Sign in and unlock this vault before connecting an organization installation")
        })?;
        let master_key = guard.master_key.ok_or_else(|| {
            anyhow!("Unlock this vault before connecting an organization installation")
        })?;
        Ok((
            guard.api.clone(),
            guard.session_generation,
            account_email,
            master_key,
        ))
    }
}

fn finish_organization_installation_connection(
    state: &AppStateWrapper,
    response: OrganizationEnrollmentRedeemResponse,
    session_generation: u64,
    account_email: String,
    master_key: [u8; 32],
) -> Result<OrganizationInstallationConnection> {
    finish_connection_with_store(
        state,
        response,
        session_generation,
        account_email,
        &INSTALLATIONS,
        |email, token| crate::auth::refresh_remembered_session_token(email, token, &master_key),
    )
}

fn finish_connection_with_store<B: InstallationBackend>(
    state: &AppStateWrapper,
    response: OrganizationEnrollmentRedeemResponse,
    session_generation: u64,
    account_email: String,
    installations: &InstallationStore<B>,
    refresh_session: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<OrganizationInstallationConnection> {
    let workspace_id = SaveStateClient::workspace_id_from_token(&response.account_token)
        .ok_or_else(|| anyhow!("The organization connection did not include a valid workspace"))?;
    let stored = StoredOrganizationInstallation {
        account_email: account_email.clone(),
        installation_id: response.installation.id.clone(),
        server_label: response.installation.server_label.clone(),
        connected_at: response.installation.connected_at.clone(),
        device_credential: response.device_credential,
        workspace_id: Some(workspace_id),
        pending_backup: None,
    };
    let metadata_warning = {
        let mut guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        if guard.session_generation != session_generation
            || guard.account_email().as_deref() != Some(account_email.as_str())
        {
            return Err(anyhow!(
                "The signed-in account changed while the installation was connecting"
            ));
        }
        installations.save(&stored).context(
            "The server connected this installation, but Windows could not save its connection. Ask your organization to replace the installation credential and issue a new recovery setup token before reconnecting",
        )?;
        let metadata_warning = crate::db::set_app_metadata(
            &guard.db,
            ORGANIZATION_INSTALLATION_METADATA_KEY,
            &stored.installation_id,
        )
        .err()
        .map(|error| {
            format!("Connected, but local installation metadata could not be saved: {error}")
        });
        guard.api.set_token(response.account_token.clone());
        guard.session_generation = guard.session_generation.wrapping_add(1);
        metadata_warning
    };
    crate::kopia::clear_session_cache();

    let session_warning = refresh_session(&account_email, &response.account_token)
        .err()
        .map(|error| {
            format!("Connected, but the refreshed sign-in could not be saved for restart: {error}")
        });
    let warnings: Vec<_> = metadata_warning
        .into_iter()
        .chain(session_warning)
        .collect();
    let persistence_warning = (!warnings.is_empty()).then(|| warnings.join(" "));

    Ok(OrganizationInstallationConnection {
        connected: true,
        installation_id: stored.installation_id,
        server_label: stored.server_label,
        connected_at: stored.connected_at,
        persistence_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryBackend {
        value: Option<StoredOrganizationInstallation>,
        fail_save: bool,
    }

    impl InstallationBackend for MemoryBackend {
        fn load(&self) -> Result<Option<StoredOrganizationInstallation>> {
            Ok(self.value.clone())
        }
        fn save(&mut self, value: &StoredOrganizationInstallation) -> Result<()> {
            if self.fail_save {
                return Err(anyhow!("simulated credential-store failure"));
            }
            self.value = Some(value.clone());
            Ok(())
        }
        fn remove(&mut self) -> Result<()> {
            self.value = None;
            Ok(())
        }
    }

    fn store(value: StoredOrganizationInstallation) -> InstallationStore<MemoryBackend> {
        InstallationStore(Mutex::new(MemoryBackend {
            value: Some(value),
            fail_save: false,
        }))
    }

    fn backup(event_id: &str) -> OrganizationBackupHeartbeat {
        OrganizationBackupHeartbeat {
            event_id: event_id.into(),
            status: "succeeded".into(),
            occurred_at: "2026-09-07T12:00:00Z".into(),
            error_code: None,
            error_reason: None,
        }
    }

    fn connection_fixture() -> (AppStateWrapper, OrganizationEnrollmentRedeemResponse) {
        use base64::Engine;
        let token = |id| {
            format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(format!("{{\"serviceId\":{id}}}"))
            )
        };
        let mut api = SaveStateClient::new("test-installation".into());
        api.set_token(token(1));
        let mut state =
            crate::state::AppState::new(api, rusqlite::Connection::open_in_memory().unwrap());
        state.email = Some("customer@example.com".into());
        state.master_key = Some([7; 32]);
        let response = OrganizationEnrollmentRedeemResponse {
            installation: crate::api::OrganizationEnrollmentInstallation {
                id: "pins_one".into(),
                server_label: "Test server".into(),
                connected_at: "2026-09-07T12:00:00Z".into(),
            },
            device_credential: "new-credential".into(),
            account_token: token(12),
        };
        (AppStateWrapper(Mutex::new(state)), response)
    }

    #[test]
    fn metadata_failure_still_finishes_session_switch_and_reports_warning() {
        let (state, response) = connection_fixture();
        // No app_metadata table: exercise the real SQLite failure after credentials save.
        let store = InstallationStore(Mutex::new(MemoryBackend::default()));
        let result = finish_connection_with_store(
            &state,
            response,
            0,
            "customer@example.com".into(),
            &store,
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(result.connected);
        assert!(result.persistence_warning.unwrap().contains("metadata"));
        assert_eq!(
            state.0.lock().unwrap().api.workspace_id().as_deref(),
            Some("service:12")
        );
        assert_eq!(
            store.load().unwrap().unwrap().workspace_id.as_deref(),
            Some("service:12")
        );
    }

    #[test]
    fn secure_store_failure_does_not_switch_workspace_or_claim_success() {
        let (state, response) = connection_fixture();
        let store = InstallationStore(Mutex::new(MemoryBackend {
            value: None,
            fail_save: true,
        }));
        let result = finish_connection_with_store(
            &state,
            response,
            0,
            "customer@example.com".into(),
            &store,
            |_, _| panic!("must not persist session"),
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("new recovery setup token"));
        assert_eq!(
            state.0.lock().unwrap().api.workspace_id().as_deref(),
            Some("service:1")
        );
        assert_eq!(state.0.lock().unwrap().session_generation, 0);
    }

    #[test]
    fn changed_account_generation_cannot_replace_saved_connection() {
        let (state, response) = connection_fixture();
        let store = store(stored("another@example.com"));
        state.0.lock().unwrap().session_generation = 1;
        let result = finish_connection_with_store(
            &state,
            response,
            0,
            "customer@example.com".into(),
            &store,
            |_, _| panic!("must not persist session"),
        );
        assert!(result.is_err());
        assert_eq!(
            store.load().unwrap().unwrap().account_email,
            "another@example.com"
        );
    }

    #[test]
    fn remembered_session_failure_keeps_active_connection_and_explains_restart() {
        let (state, response) = connection_fixture();
        state
            .0
            .lock()
            .unwrap()
            .db
            .execute_batch("CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        let store = InstallationStore(Mutex::new(MemoryBackend::default()));
        let result = finish_connection_with_store(
            &state,
            response,
            0,
            "customer@example.com".into(),
            &store,
            |_, _| Err(anyhow!("simulated restart-store failure")),
        )
        .unwrap();
        assert!(result.connected);
        assert!(result.persistence_warning.unwrap().contains("restart"));
        assert_eq!(
            state.0.lock().unwrap().api.workspace_id().as_deref(),
            Some("service:12")
        );
    }

    #[test]
    fn backup_health_uses_the_exact_account_and_workspace_scope() {
        let store = store(stored("customer@example.com"));
        for scope in [
            "customer@example.com",
            "customer@example.com::service:99",
            "other@example.com::service:12",
        ] {
            assert!(store
                .queue(scope, &backup("wrong-workspace"))
                .unwrap()
                .is_none());
        }
        let sent = store
            .queue(
                "customer@example.com::service:12",
                &backup("correct-workspace"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(sent.pending_backup.unwrap().event_id, "correct-workspace");
    }

    #[test]
    fn old_revocation_cannot_delete_a_reconnected_installation() {
        let old = stored("customer@example.com");
        let store = store(old.clone());
        let mut replacement = old.clone();
        replacement.device_credential = "new-device-credential".into();
        store.save(&replacement).unwrap();
        store.remove_if_current(&old).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().device_credential,
            "new-device-credential"
        );
        store.remove_if_current(&replacement).unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn acknowledgment_preserves_newer_pending_backup_and_replacement_binding() {
        let store = store(stored("customer@example.com"));
        let old = store
            .queue("customer@example.com::service:12", &backup("old-event"))
            .unwrap()
            .unwrap();
        let newer = store
            .queue("customer@example.com::service:12", &backup("new-event"))
            .unwrap()
            .unwrap();
        store.acknowledge(&old, Some("old-event"), None).unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .unwrap()
                .pending_backup
                .unwrap()
                .event_id,
            "new-event"
        );
        let mut replacement = newer.clone();
        replacement.device_credential = "replacement".into();
        store.save(&replacement).unwrap();
        store.acknowledge(&newer, Some("new-event"), None).unwrap();
        assert!(store.load().unwrap().unwrap().pending_backup.is_some());
        store
            .acknowledge(&replacement, Some("new-event"), None)
            .unwrap();
        assert!(store.load().unwrap().unwrap().pending_backup.is_none());
    }

    #[test]
    fn legacy_binding_learns_workspace_from_authenticated_heartbeat_only() {
        let mut legacy = stored("customer@example.com");
        legacy.workspace_id = None;
        let store = store(legacy.clone());
        assert!(store
            .queue("customer@example.com::service:12", &backup("unproven"))
            .unwrap()
            .is_none());
        store
            .acknowledge(&legacy, None, Some("service:12"))
            .unwrap();
        assert!(store
            .queue("customer@example.com::service:12", &backup("proven"))
            .unwrap()
            .is_some());
        store
            .acknowledge(&legacy, None, Some("service:99"))
            .unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().workspace_id.as_deref(),
            Some("service:12")
        );
    }

    #[test]
    fn failed_credential_writes_preserve_the_last_saved_state() {
        let old = stored("customer@example.com");
        let store = store(old.clone());
        store.0.lock().unwrap().fail_save = true;
        assert!(store
            .queue("customer@example.com::service:12", &backup("unsaved"))
            .is_err());
        let mut replacement = old.clone();
        replacement.device_credential = "unsaved-replacement".into();
        assert!(store.save(&replacement).is_err());
        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.device_credential, old.device_credential);
        assert!(saved.pending_backup.is_none());
    }

    #[test]
    fn concurrent_queue_and_acknowledgment_never_drop_a_new_event() {
        let store = std::sync::Arc::new(store(stored("customer@example.com")));
        for index in 0..100 {
            let old = store
                .queue("customer@example.com::service:12", &backup("old"))
                .unwrap()
                .unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let worker_store = store.clone();
            let worker_barrier = barrier.clone();
            let thread = std::thread::spawn(move || {
                worker_barrier.wait();
                worker_store.acknowledge(&old, Some("old"), None).unwrap();
            });
            barrier.wait();
            let event_id = format!("new-{index}");
            store
                .queue("customer@example.com::service:12", &backup(&event_id))
                .unwrap();
            thread.join().unwrap();
            assert_eq!(
                store
                    .load()
                    .unwrap()
                    .unwrap()
                    .pending_backup
                    .unwrap()
                    .event_id,
                event_id
            );
        }
    }

    fn stored(account_email: &str) -> StoredOrganizationInstallation {
        StoredOrganizationInstallation {
            account_email: account_email.into(),
            installation_id: "pins_one".into(),
            server_label: "Windows Prod 01".into(),
            connected_at: "2026-08-29T20:00:00.000Z".into(),
            device_credential: "secret-never-returned-in-status".into(),
            workspace_id: Some("service:12".into()),
            pending_backup: None,
        }
    }

    #[test]
    fn connected_status_is_scoped_to_the_active_account_and_hides_the_credential() {
        let value = stored("customer@example.com");
        let status = status_for_account(Some(&value), Some("Customer@Example.com"));
        assert!(status.connected);
        assert_eq!(status.installation_id.as_deref(), Some("pins_one"));
        assert!(!serde_json::to_string(&status)
            .unwrap()
            .contains("secret-never-returned"));

        let other = status_for_account(Some(&value), Some("other@example.com"));
        assert!(!other.connected);
        assert!(other.installation_id.is_none());
    }

    #[test]
    fn stored_device_binding_round_trips_without_plaintext_files() {
        let mut value = stored("customer@example.com");
        value.pending_backup = Some(OrganizationBackupHeartbeat {
            event_id: "11111111-1111-4111-8111-111111111111".into(),
            status: "succeeded".into(),
            occurred_at: "2026-08-29T20:01:00.000Z".into(),
            error_code: None,
            error_reason: None,
        });
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: StoredOrganizationInstallation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.installation_id, "pins_one");
        assert_eq!(decoded.device_credential, "secret-never-returned-in-status");
        assert_eq!(
            decoded.pending_backup.unwrap().event_id,
            "11111111-1111-4111-8111-111111111111"
        );
    }

    #[test]
    fn only_the_stable_invalid_credential_response_triggers_local_removal() {
        assert!(device_credential_was_revoked(&anyhow!(String::from(
            "Organization installation heartbeat failed (401 Unauthorized): {\"error\":\"invalid_device_credential\"}"
        ))));
        assert!(!device_credential_was_revoked(&anyhow!(
            "Organization installation heartbeat failed (503 Service Unavailable)"
        )));
    }
}
