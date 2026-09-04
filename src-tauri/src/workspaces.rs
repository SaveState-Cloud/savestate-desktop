use crate::api::{AccountWorkspace, AccountWorkspacesResponse};
use crate::backup_operations::AccountContext;
use crate::state::AppStateWrapper;
use anyhow::{anyhow, bail, Result};

fn workspace_scope(email: &str, workspace_id: &str) -> String {
    format!("{}::{}", email.trim().to_ascii_lowercase(), workspace_id)
}

/// Build independent API contexts for every usable workspace owned by the
/// signed-in account. This does not mutate the token used by the visible UI.
pub async fn scheduler_account_contexts(state: &AppStateWrapper) -> Result<Vec<AccountContext>> {
    let (api, email, master_key, session_generation) = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        (
            guard.api.clone(),
            guard
                .account_email()
                .ok_or_else(|| anyhow!("Sign in before running scheduled backups"))?,
            guard
                .master_key
                .ok_or_else(|| anyhow!("Unlock the vault before running scheduled backups"))?,
            guard.session_generation,
        )
    };
    let response = api.get_account_workspaces().await?;
    if let Some(personal) = response
        .workspaces
        .iter()
        .find(|workspace| workspace.kind == "personal")
    {
        let personal_scope = workspace_scope(&email, &personal.id);
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        crate::db::migrate_legacy_account_profiles_to_workspace(
            &guard.db,
            &email,
            &personal_scope,
        )?;
    }
    let current_workspace = api.workspace_id();
    let mut contexts = Vec::new();

    for workspace in response
        .workspaces
        .into_iter()
        .filter(|item| item.available)
    {
        let scoped_api = if current_workspace.as_deref() == Some(workspace.id.as_str()) {
            api.clone()
        } else {
            let mut scoped_api = api.clone();
            match api.switch_account_workspace(&workspace.id).await {
                Ok(switched) => scoped_api.set_token(switched.token),
                Err(error) => {
                    eprintln!(
                        "Scheduled workspace {} became unavailable: {error}",
                        workspace.id
                    );
                    continue;
                }
            }
            scoped_api
        };
        contexts.push(AccountContext {
            api: scoped_api,
            account_scope: workspace_scope(&email, &workspace.id),
            repository_password: hex::encode(master_key),
            session_generation,
        });
    }

    let guard = state
        .0
        .lock()
        .map_err(|error| anyhow!("Lock error: {error}"))?;
    if guard.session_generation != session_generation
        || guard.account_email().as_deref() != Some(email.as_str())
    {
        bail!("The signed-in account changed while schedules were loading");
    }
    Ok(contexts)
}

#[tauri::command]
pub async fn cmd_list_account_workspaces(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<AccountWorkspacesResponse, String> {
    list_account_workspaces(state.inner())
        .await
        .map_err(|error| error.to_string())
}

async fn list_account_workspaces(state: &AppStateWrapper) -> Result<AccountWorkspacesResponse> {
    let (api, email) = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        let email = guard
            .account_email()
            .ok_or_else(|| anyhow!("Sign in before viewing workspaces"))?;
        (guard.api.clone(), email)
    };
    let response = api.get_account_workspaces().await?;
    if let Some(personal) = response
        .workspaces
        .iter()
        .find(|workspace| workspace.kind == "personal")
    {
        let scope = workspace_scope(&email, &personal.id);
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        crate::db::migrate_legacy_account_profiles_to_workspace(&guard.db, &email, &scope)?;
    }
    Ok(response)
}

#[tauri::command]
pub async fn cmd_switch_account_workspace(
    state: tauri::State<'_, AppStateWrapper>,
    workspace_id: String,
) -> std::result::Result<AccountWorkspace, String> {
    switch_account_workspace(state.inner(), workspace_id.trim())
        .await
        .map_err(|error| error.to_string())
}

async fn switch_account_workspace(
    state: &AppStateWrapper,
    workspace_id: &str,
) -> Result<AccountWorkspace> {
    let _session_change = crate::backup_operations::begin_session_change()?;
    let (api, email, master_key, session_generation) = {
        let guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        (
            guard.api.clone(),
            guard
                .account_email()
                .ok_or_else(|| anyhow!("Sign in before switching workspaces"))?,
            guard
                .master_key
                .ok_or_else(|| anyhow!("Unlock the vault before switching workspaces"))?,
            guard.session_generation,
        )
    };
    if api.workspace_id().as_deref() == Some(workspace_id) {
        let current = api
            .get_account_workspaces()
            .await?
            .workspaces
            .into_iter()
            .find(|workspace| workspace.current)
            .ok_or_else(|| anyhow!("The current workspace is unavailable"))?;
        return Ok(current);
    }
    let response = api.switch_account_workspace(workspace_id).await?;
    {
        let mut guard = state
            .0
            .lock()
            .map_err(|error| anyhow!("Lock error: {error}"))?;
        if guard.session_generation != session_generation
            || guard.account_email().as_deref() != Some(email.as_str())
        {
            bail!("The signed-in account changed while the workspace was switching");
        }
        guard.api.set_token(response.token.clone());
        guard.session_generation = guard.session_generation.wrapping_add(1);
    }
    crate::kopia::clear_session_cache();
    crate::auth::refresh_remembered_session_token(&email, &response.token, &master_key)?;
    Ok(response.workspace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_profile_scope_keeps_services_separate() {
        assert_eq!(
            workspace_scope("Owner@Example.com", "service:12"),
            "owner@example.com::service:12"
        );
    }
}
