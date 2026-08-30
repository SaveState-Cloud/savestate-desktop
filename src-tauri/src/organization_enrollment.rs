use crate::api::{
    OrganizationAvailableInstallationsResponse, OrganizationBackupHeartbeat,
    OrganizationEnrollmentPreviewResponse, OrganizationEnrollmentRedeemResponse, SaveStateClient,
};
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

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
    keyring::v1::Entry::new("SaveState Vault", "organization-installation")
        .context("Windows Credential Manager is unavailable")
}

fn save_installation(value: &StoredOrganizationInstallation) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    credential_entry()?
        .set_secret(&data)
        .context("Failed to save the organization device credential securely")
}

fn load_installation() -> Option<StoredOrganizationInstallation> {
    let data = credential_entry().ok()?.get_secret().ok()?;
    serde_json::from_slice(&data).ok()
}

fn remove_installation() -> Result<()> {
    credential_entry()?
        .delete_credential()
        .context("Failed to remove the disabled organization device credential")
}

fn device_credential_was_revoked(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("invalid_device_credential"))
}

fn installation_for_account(account_email: &str) -> Option<StoredOrganizationInstallation> {
    let stored = load_installation()?;
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

fn clear_delivered_backup(account_email: &str, event_id: &str) {
    let Some(mut stored) = installation_for_account(account_email) else {
        return;
    };
    if stored
        .pending_backup
        .as_ref()
        .map(|backup| backup.event_id.as_str())
        != Some(event_id)
    {
        return;
    }
    stored.pending_backup = None;
    if let Err(error) = save_installation(&stored) {
        eprintln!("Failed to clear delivered organization backup health: {error}");
    }
}

pub(crate) fn queue_organization_installation_backup_heartbeat(
    api: &SaveStateClient,
    account_email: &str,
    backup: OrganizationBackupHeartbeat,
) {
    let Some(mut stored) = installation_for_account(account_email) else {
        return;
    };
    stored.pending_backup = Some(backup.clone());
    if let Err(error) = save_installation(&stored) {
        eprintln!("Failed to persist organization backup health for retry: {error}");
    }
    let api = api.clone();
    let account_email = account_email.to_string();
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
                Ok(()) => {
                    clear_delivered_backup(&account_email, &event_id);
                    return;
                }
                Err(error) => {
                    if device_credential_was_revoked(&error) {
                        if let Err(remove_error) = remove_installation() {
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
        let Some(account_email) = guard.account_scope() else {
            return Ok(());
        };
        (guard.api.clone(), account_email)
    };
    let Some(stored) = installation_for_account(&account_email) else {
        return Ok(());
    };
    let pending_backup = stored.pending_backup.clone();
    if let Err(error) = api
        .organization_installation_heartbeat(&stored.device_credential, pending_backup.clone())
        .await
    {
        if device_credential_was_revoked(&error) {
            remove_installation()?;
            return Ok(());
        }
        return Err(error);
    }
    if let Some(backup) = pending_backup {
        clear_delivered_backup(&account_email, &backup.event_id);
    }
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
        .account_scope();
    Ok(status_for_account(
        load_installation().as_ref(),
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
        if guard.account_scope().is_none() {
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
        if guard.account_scope().is_none() {
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
        let account_email = guard.account_scope().ok_or_else(|| {
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
    {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        if guard.session_generation != session_generation
            || guard.account_scope().as_deref() != Some(account_email.as_str())
        {
            return Err(anyhow!(
                "The signed-in account changed while the installation was connecting"
            ));
        }
    }

    let stored = StoredOrganizationInstallation {
        account_email: account_email.clone(),
        installation_id: response.installation.id.clone(),
        server_label: response.installation.server_label.clone(),
        connected_at: response.installation.connected_at.clone(),
        device_credential: response.device_credential,
        pending_backup: None,
    };
    save_installation(&stored)?;

    {
        let mut guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        if guard.session_generation != session_generation
            || guard.account_scope().as_deref() != Some(account_email.as_str())
        {
            return Err(anyhow!(
                "The signed-in account changed while the installation was connecting"
            ));
        }
        guard.api.set_token(response.account_token.clone());
        guard.session_generation = guard.session_generation.wrapping_add(1);
        crate::db::set_app_metadata(
            &guard.db,
            ORGANIZATION_INSTALLATION_METADATA_KEY,
            &stored.installation_id,
        )?;
    }
    crate::kopia::clear_session_cache();

    let persistence_warning = crate::auth::refresh_remembered_session_token(
        &account_email,
        &response.account_token,
        &master_key,
    )
    .err()
    .map(|error| {
        format!("Connected, but the refreshed sign-in could not be saved for restart: {error}")
    });

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

    fn stored(account_email: &str) -> StoredOrganizationInstallation {
        StoredOrganizationInstallation {
            account_email: account_email.into(),
            installation_id: "pins_one".into(),
            server_label: "Windows Prod 01".into(),
            connected_at: "2026-08-29T20:00:00.000Z".into(),
            device_credential: "secret-never-returned-in-status".into(),
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
