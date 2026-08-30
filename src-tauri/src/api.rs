use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// ────────────────────────────────────────────────────────────────────
// Response types
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub token: String,
    #[serde(default)]
    pub email: Option<String>,
    pub encrypted_master_key: Option<String>,
    #[serde(default)]
    pub encrypted_master_key_envelope: Option<String>,
    #[serde(default)]
    pub master_key_requires_original_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountResponse {
    pub email: String,
    pub bucket: Option<String>,
    #[serde(
        alias = "storageLimitGB",
        alias = "storageLimitGb",
        alias = "storage_limit_gb"
    )]
    pub storage_limit_gb: Option<f64>,
    #[serde(default, alias = "profileLimit", alias = "profile_limit")]
    pub profile_limit: Option<u32>,
    pub usage: Option<serde_json::Value>,
    #[serde(default)]
    pub ingress: Option<serde_json::Value>,
    #[serde(default)]
    pub egress: Option<serde_json::Value>,
    pub status: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default, alias = "serviceId")]
    pub service_id: Option<serde_json::Value>,
    #[serde(default, alias = "trialEndsAt")]
    pub trial_ends_at: Option<String>,
    #[serde(default, alias = "currentPeriodEnd")]
    pub current_period_end: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitlementResponse {
    #[serde(default, alias = "profileLimit", alias = "profile_limit")]
    pub profile_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresignResponse {
    pub upload_url: String,
    pub key: String,
    pub expires_in: u64,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultipartPresignResponse {
    pub upload_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub key: String,
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    #[serde(rename = "sizeFormatted")]
    pub size_formatted: Option<String>,
    #[serde(default)]
    #[serde(rename = "lastModified")]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupListResponse {
    pub backups: Vec<BackupEntry>,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadResponse {
    pub download_url: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResponse {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterKeyResponse {
    pub encrypted_master_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterKeyEnvelopeResponse {
    pub envelope: Option<String>,
    pub version: Option<u32>,
    pub revision: Option<u64>,
    pub legacy_encrypted_master_key: Option<String>,
    #[serde(default)]
    pub account_recovery_changed_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterKeyEnvelopeWriteResponse {
    pub success: bool,
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub initialized: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPrefs {
    #[serde(default = "default_true")]
    pub backup_success: bool,
    #[serde(default = "default_true")]
    pub backup_failure: bool,
    #[serde(default = "default_true")]
    pub restore_success: bool,
    #[serde(default = "default_true")]
    pub restore_failure: bool,
    #[serde(default = "default_true")]
    pub backup_scheduled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            backup_success: true,
            backup_failure: true,
            restore_success: true,
            restore_failure: true,
            backup_scheduled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSettings {
    pub discord_webhook_url: Option<String>,
    #[serde(default)]
    pub clear_discord_webhook: bool,
    #[serde(default)]
    pub discord_webhook_configured: bool,
    pub notification_prefs: NotificationPrefs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericSuccess {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountWorkspace {
    pub id: String,
    pub service_id: u64,
    pub kind: String,
    pub label: String,
    pub organization_id: Option<String>,
    pub customer_id: Option<String>,
    pub plan: String,
    pub service_status: String,
    pub lifecycle_state: Option<String>,
    pub storage_limit_bytes: u64,
    pub profile_limit: u32,
    pub unlimited: bool,
    pub available: bool,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountWorkspacesResponse {
    pub workspaces: Vec<AccountWorkspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountWorkspaceSwitchResponse {
    pub token: String,
    pub workspace: AccountWorkspace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentPreviewResponse {
    pub enrollment: OrganizationEnrollmentPreview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentPreview {
    pub organization: OrganizationEnrollmentOrganization,
    pub customer: OrganizationEnrollmentCustomer,
    pub installation: OrganizationEnrollmentInstallationPreview,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationEnrollmentOrganization {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentCustomer {
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentInstallationPreview {
    pub server_label: String,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentRedeemResponse {
    pub installation: OrganizationEnrollmentInstallation,
    pub device_credential: String,
    pub account_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationEnrollmentInstallation {
    pub id: String,
    pub server_label: String,
    pub connected_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationAvailableInstallationsResponse {
    pub installations: Vec<OrganizationAvailableInstallation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationAvailableInstallation {
    pub id: String,
    pub organization_name: String,
    pub customer_name: String,
    pub server_label: String,
    pub platform: String,
    pub quota_bytes: u64,
    pub service_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationBackupHeartbeat {
    pub event_id: String,
    pub status: String,
    pub occurred_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
}

/// Privacy-safe schedule metadata sent to Engine. Profile names and source
/// paths are deliberately excluded.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineScheduleSnapshot {
    pub profile_id: String,
    pub profile_kind: String,
    pub times: Vec<String>,
    pub interval_days: u32,
    pub next_run_at: Option<String>,
    pub retry_at: Option<String>,
    pub retry_count: u32,
    pub state: String,
    pub last_error_code: Option<String>,
    pub enabled: bool,
}

// ────────────────────────────────────────────────────────────────────
// API error wrapper
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

async fn parse_api_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if let Ok(api_err) = serde_json::from_str::<ApiError>(&text) {
            let message = api_err
                .message
                .or(api_err.error)
                .unwrap_or_else(|| format!("{} failed ({})", operation, status));
            return Err(anyhow!(message));
        }
        return Err(anyhow!("{} failed: {} — {}", operation, status, text));
    }
    serde_json::from_str(&text).with_context(|| format!("Failed to parse {} response", operation))
}

async fn parse_organization_enrollment_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if let Ok(api_error) = serde_json::from_str::<ApiError>(&text) {
            let code = api_error
                .error
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "organization_enrollment_failed".to_string());
            let message = api_error
                .message
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("{} failed ({})", operation, status));
            return Err(anyhow!("{}: {}", code, message));
        }
        return Err(anyhow!("{} failed ({})", operation, status));
    }
    serde_json::from_str(&text).with_context(|| format!("Failed to parse {} response", operation))
}

// ────────────────────────────────────────────────────────────────────
// Client
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SaveStateClient {
    pub base_url: String,
    pub token: Option<String>,
    pub installation_id: String,
    client: Client,
    transfer_client: Client,
}

#[derive(serde::Deserialize, Debug)]
pub struct MultipartCreateResponse {
    #[serde(rename = "uploadId")]
    pub upload_id: String,
    pub key: String,
}

#[derive(serde::Serialize, Debug)]
pub struct MultipartPart {
    #[serde(rename = "partNumber")]
    pub part_number: u32,
    #[serde(rename = "eTag")]
    pub etag: String,
}

/// Short-lived credentials for SaveState's ciphertext-only repository gateway.
/// They authorize one account-scoped operation; Backblaze provider credentials
/// never reach the desktop agent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSession {
    pub mode: String,
    pub bucket: String,
    pub prefix: String,
    pub endpoint: String,
    #[serde(default)]
    pub endpoint_host: Option<String>,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub expires_in: u64,
}

/// One-time grant returned after the backend has atomically authorized a free
/// restore and recorded byte-exact operational telemetry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreAuthorization {
    pub authorized: bool,
    #[serde(default)]
    pub grant_id: Option<String>,
    #[serde(default)]
    pub grant_expires_at: Option<String>,
}

/// Result of a server-side FIFO retention pass.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionResult {
    #[serde(default)]
    pub pruned_count: u64,
    #[serde(default)]
    pub usage: u64,
    #[serde(default)]
    pub within_quota: bool,
    #[serde(default)]
    pub percent_used: f64,
    #[serde(default)]
    pub pressure_level: String,
    #[serde(default)]
    pub maintenance_recommended: bool,
    #[serde(default)]
    pub maintenance_urgent: bool,
}

impl SaveStateClient {
    pub fn new(installation_id: String) -> Self {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            HeaderName::from_static("x-savestate-client-id"),
            HeaderValue::from_str(&installation_id).expect("installation ID is a valid header"),
        );
        default_headers.insert(
            HeaderName::from_static("x-savestate-app-version"),
            HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
        );
        default_headers.insert(
            HeaderName::from_static("x-savestate-platform"),
            HeaderValue::from_static(std::env::consts::OS),
        );
        Self {
            base_url: "https://api.savestate.dk".to_string(),
            token: None,
            installation_id,
            client: Client::builder()
                .default_headers(default_headers)
                .timeout(std::time::Duration::from_secs(600))
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            // Presigned Backblaze transfers must not receive the installation
            // telemetry headers that are scoped to api.savestate.dk.
            transfer_client: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build transfer HTTP client"),
        }
    }

    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    pub fn clear_token(&mut self) {
        self.token = None;
    }

    pub fn workspace_id(&self) -> Option<String> {
        let payload = self.token.as_deref()?.split('.').nth(1)?;
        let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        let service_id = claims.get("serviceId")?.as_u64()?;
        (service_id > 0).then(|| format!("service:{service_id}"))
    }

    fn auth_header(&self) -> Result<String> {
        self.token
            .as_ref()
            .map(|t| format!("Bearer {}", t))
            .ok_or_else(|| anyhow!("Not authenticated"))
    }

    pub async fn inspect_organization_installation(
        &self,
        setup_token: &str,
    ) -> Result<OrganizationEnrollmentPreviewResponse> {
        let response = self
            .client
            .post(format!(
                "{}/organization/installations/inspect",
                self.base_url
            ))
            .header("Authorization", self.auth_header()?)
            .json(&serde_json::json!({ "token": setup_token }))
            .send()
            .await
            .context("Failed to inspect the organization installation")?;
        parse_organization_enrollment_json(response, "Organization installation review").await
    }

    pub async fn available_organization_installations(
        &self,
    ) -> Result<OrganizationAvailableInstallationsResponse> {
        let response = self
            .client
            .get(format!(
                "{}/organization/installations/available",
                self.base_url
            ))
            .header("Authorization", self.auth_header()?)
            .send()
            .await
            .context("Failed to find organization installations for this account")?;
        parse_organization_enrollment_json(response, "Organization installation discovery").await
    }

    pub async fn connect_organization_installation(
        &self,
        installation_id: &str,
    ) -> Result<OrganizationEnrollmentRedeemResponse> {
        let response = self
            .client
            .post(format!(
                "{}/organization/installations/connect",
                self.base_url
            ))
            .header("Authorization", self.auth_header()?)
            .json(&serde_json::json!({
                "installationId": installation_id,
                "deviceId": self.installation_id,
            }))
            .send()
            .await
            .context("Failed to connect the organization installation")?;
        parse_organization_enrollment_json(response, "Organization installation connection").await
    }

    pub async fn redeem_organization_installation(
        &self,
        setup_token: &str,
    ) -> Result<OrganizationEnrollmentRedeemResponse> {
        let response = self
            .client
            .post(format!(
                "{}/organization/installations/redeem",
                self.base_url
            ))
            .header("Authorization", self.auth_header()?)
            .json(&serde_json::json!({
                "token": setup_token,
                "deviceId": self.installation_id,
            }))
            .send()
            .await
            .context("Failed to connect the organization installation")?;
        parse_organization_enrollment_json(response, "Organization installation connection").await
    }

    pub async fn organization_installation_heartbeat(
        &self,
        device_credential: &str,
        backup: Option<OrganizationBackupHeartbeat>,
    ) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "{}/organization/installations/heartbeat",
                self.base_url
            ))
            .header("Authorization", format!("Bearer {device_credential}"))
            .timeout(std::time::Duration::from_secs(15))
            .json(&serde_json::json!({ "backup": backup }))
            .send()
            .await
            .context("Failed to report organization installation health")?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(anyhow!(
            "Organization installation heartbeat failed ({status}): {text}"
        ))
    }

    /// Best-effort metadata-only job lifecycle reporting. This is intentionally
    /// detached from the customer operation so Engine downtime cannot delay or
    /// fail a backup, restore, cleanup, or update.
    pub fn report_job_event(
        &self,
        job_id: &str,
        kind: &str,
        status: &str,
        stage: Option<&str>,
        trigger: Option<&str>,
        bytes: Option<u64>,
        files: Option<u64>,
        error_code: Option<&str>,
        error_message: Option<&str>,
        started_at: &str,
    ) {
        let Ok(auth) = self.auth_header() else { return };
        let client = self.client.clone();
        let url = format!("{}/engine/jobs", self.base_url);
        let occurred_at = chrono::Utc::now().to_rfc3339();
        let body = serde_json::json!({
            "jobId": job_id,
            "kind": kind,
            "status": status,
            "stage": stage,
            "trigger": trigger,
            "bytes": bytes,
            "files": files,
            "errorCode": error_code,
            "errorMessage": error_message,
            "occurredAt": occurred_at,
            "startedAt": started_at,
            "completedAt": if matches!(status, "succeeded" | "failed" | "cancelled" | "warning" | "interrupted") {
                Some(chrono::Utc::now().to_rfc3339())
            } else {
                None
            },
        });
        tokio::spawn(async move {
            let _ = client
                .post(url)
                .header("Authorization", auth)
                .json(&body)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        });
    }

    /// Send the full schedule snapshot and acknowledge only an accepted
    /// response. The caller keeps this off the backup critical path and uses
    /// the result to retry failed telemetry instead of suppressing it for 15
    /// minutes.
    pub async fn send_schedule_snapshot(
        &self,
        schedules: Vec<EngineScheduleSnapshot>,
    ) -> Result<()> {
        let auth = self.auth_header()?;
        let url = format!("{}/engine/schedules", self.base_url);
        let body = serde_json::json!({ "schedules": schedules });
        let response = self
            .client
            .post(url)
            .header("Authorization", auth)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .context("Failed to report schedules")?;
        if !response.status().is_success() {
            return Err(anyhow!("Schedule report returned {}", response.status()));
        }
        Ok(())
    }

    // ── Login ───────────────────────────────────────────────────────

    pub async fn login(&mut self, email: &str, password: &str) -> Result<LoginResponse> {
        let url = format!("{}/auth/login", self.base_url);
        let body = serde_json::json!({
            "email": email,
            "password": password,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send login request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if let Ok(api_err) = serde_json::from_str::<ApiError>(&text) {
                let msg = api_err
                    .error
                    .or(api_err.message)
                    .unwrap_or_else(|| format!("Login failed ({})", status));
                return Err(anyhow!(msg));
            }
            return Err(anyhow!("Login failed: {} — {}", status, text));
        }

        let login_resp: LoginResponse = resp
            .json()
            .await
            .context("Failed to parse login response")?;
        self.token = Some(login_resp.token.clone());
        Ok(login_resp)
    }

    // ── Account ─────────────────────────────────────────────────────

    pub async fn get_account(&self) -> Result<AccountResponse> {
        let url = format!("{}/account", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to fetch account info")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Get account failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse account response")
    }

    pub async fn get_entitlements(&self) -> Result<EntitlementResponse> {
        let url = format!("{}/account/entitlements", self.base_url);
        let auth = self.auth_header()?;
        let response = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to fetch account entitlements")?;
        parse_api_json(response, "Get account entitlements").await
    }

    pub async fn get_account_workspaces(&self) -> Result<AccountWorkspacesResponse> {
        let response = self
            .client
            .get(format!("{}/account/workspaces", self.base_url))
            .header("Authorization", self.auth_header()?)
            .send()
            .await
            .context("Failed to fetch account workspaces")?;
        parse_api_json(response, "Get account workspaces").await
    }

    pub async fn switch_account_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<AccountWorkspaceSwitchResponse> {
        let response = self
            .client
            .post(format!("{}/account/workspaces/switch", self.base_url))
            .header("Authorization", self.auth_header()?)
            .json(&serde_json::json!({ "workspaceId": workspace_id }))
            .send()
            .await
            .context("Failed to switch account workspace")?;
        parse_api_json(response, "Switch account workspace").await
    }

    // ── Phase 1: Kopia repository session (short-lived B2 creds) ─────

    /// Request short-lived, account-scoped repository-gateway credentials for
    /// the Kopia engine. These are not Backblaze provider credentials.
    /// `mode` is "backup" (read/write) or "restore" (read-only).
    pub async fn repo_session(&self, mode: &str, grant_id: Option<&str>) -> Result<RepoSession> {
        let url = format!("{}/repo/session", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "mode": mode, "grantId": grant_id }))
            .send()
            .await
            .context("Failed to request repository session")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Repo session failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse repo session response")
    }

    // ── Free restore authorization ──────────────────────────────────

    /// Ask the backend to authorize a restore. `bytes` remains telemetry only;
    /// restores are never blocked by a transfer allowance or billed as overage.
    pub async fn restore_authorize(
        &self,
        snapshot_id: &str,
        bytes: u64,
    ) -> Result<RestoreAuthorization> {
        let url = format!("{}/restore/authorize", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({
                "snapshotId": snapshot_id,
                "bytes": bytes,
                "reservationId": uuid::Uuid::new_v4().to_string(),
            }))
            .send()
            .await
            .context("Failed to request restore authorization")?;

        if resp.status().is_success() {
            return resp
                .json()
                .await
                .context("Failed to parse restore authorization");
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Surface a human-readable authorization error when present.
        if let Ok(api_err) = serde_json::from_str::<ApiError>(&text) {
            if let Some(msg) = api_err.message.or(api_err.error) {
                return Err(anyhow!(msg));
            }
        }
        Err(anyhow!(
            "Restore authorization failed: {} — {}",
            status,
            text
        ))
    }

    // ── Phase 4: server-side FIFO retention enforcement ──────────────

    pub async fn enforce_retention(&self) -> Result<RetentionResult> {
        let url = format!("{}/backup/enforce-retention", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to enforce retention")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Enforce retention failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse retention response")
    }

    // ── Presign Upload ──────────────────────────────────────────────

    pub async fn presign_upload(
        &self,
        filename: &str,
        size: u64,
        content_type: &str,
        folder: Option<&str>,
    ) -> Result<PresignResponse> {
        let url = format!("{}/backup/presign", self.base_url);
        let auth = self.auth_header()?;

        let mut body = serde_json::json!({
            "filename": filename,
            "size": size,
            "contentType": content_type,
        });
        if let Some(f) = folder {
            body["folder"] = serde_json::Value::String(f.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to request presigned upload URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Presign upload failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse presign upload response")
    }

    // ── Multipart Upload APIs ──────────────────────────────────────

    pub async fn multipart_create(
        &self,
        filename: &str,
        size: u64,
        content_type: &str,
        folder: Option<&str>,
    ) -> Result<MultipartCreateResponse> {
        let url = format!("{}/backup/multipart/create", self.base_url);
        let auth = self.auth_header()?;

        let mut body = serde_json::json!({
            "filename": filename,
            "size": size,
            "contentType": content_type,
        });
        if let Some(f) = folder {
            body["folder"] = serde_json::Value::String(f.to_string());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to create multipart upload")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Multipart create failed: {} — {}", status, text));
        }

        resp.json().await.context("Parse multipart create failed")
    }

    pub async fn multipart_presign_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
    ) -> Result<MultipartPresignResponse> {
        let url = format!("{}/backup/multipart/presign-part", self.base_url);
        let auth = self.auth_header()?;

        let body = serde_json::json!({
            "key": key,
            "uploadId": upload_id,
            "partNumber": part_number,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to presign multipart part")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Presign part failed: {} — {}", status, text));
        }

        resp.json().await.context("Parse presign part failed")
    }

    pub async fn multipart_complete(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<MultipartPart>,
    ) -> Result<()> {
        let url = format!("{}/backup/multipart/complete", self.base_url);
        let auth = self.auth_header()?;

        let body = serde_json::json!({
            "key": key,
            "uploadId": upload_id,
            "parts": parts,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to complete multipart upload")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Multipart complete failed: {} — {}", status, text));
        }

        Ok(())
    }

    pub async fn presign_manifest(
        &self,
        exact_key: &str,
        content_type: &str,
    ) -> Result<PresignResponse> {
        let url = format!("{}/backup/manifest/upload", self.base_url);
        let auth = self.auth_header()?;

        let body = serde_json::json!({
            "exactKey": exact_key,
            "contentType": content_type,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to get manifest presigned URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Manifest presign failed: {} — {}", status, text));
        }

        resp.json().await.context("Parse manifest presign failed")
    }

    // ── List Backups ────────────────────────────────────────────────

    pub async fn list_backups(&self) -> Result<BackupListResponse> {
        let url = format!("{}/backup/list", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to list backups")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("List backups failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse backup list response")
    }

    // ── Presign Download ────────────────────────────────────────────

    pub async fn presign_download(&self, key: &str) -> Result<DownloadResponse> {
        let url = format!("{}/backup/presign-download", self.base_url);
        let auth = self.auth_header()?;

        let body = serde_json::json!({ "key": key });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to request presigned download URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Presign download failed: {} — {}", status, text));
        }

        resp.json()
            .await
            .context("Failed to parse presign download response")
    }

    // ── Delete Backup ───────────────────────────────────────────────

    pub async fn delete_backup(&self, key: &str) -> Result<()> {
        let url = format!("{}/backup/{}", self.base_url, key);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .delete(url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to delete backup")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Delete backup failed: {} — {}", status, text));
        }

        Ok(())
    }

    // ── Cancel Subscription ─────────────────────────────────────────

    pub async fn cancel_subscription(&self) -> Result<CancelResponse> {
        let url = format!("{}/account/cancel", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to cancel subscription")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Cancel subscription failed: {} — {}", status, text));
        }

        resp.json().await.context("Failed to parse cancel response")
    }

    // ── Resume Subscription ─────────────────────────────────────────

    pub async fn resume_subscription(&self) -> Result<serde_json::Value> {
        let url = format!("{}/account/resume", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to resume subscription")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Resume subscription failed: {} — {}", status, text));
        }

        resp.json().await.context("Failed to parse resume response")
    }

    // ── Raw upload to presigned URL ─────────────────────────────────

    pub async fn upload_to_presigned_url(
        &self,
        url: &str,
        data: Vec<u8>,
        content_type: &str,
        _size_bytes: u64,
    ) -> Result<()> {
        // Generous timeout: assume worst-case 1 MB/s, minimum 600s (10 min)
        let data_len = data.len() as u64;
        let calculated_secs = (data_len / (1024 * 1024)).saturating_add(60);
        let timeout_secs = std::cmp::max(600, calculated_secs);

        let upload_client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to build upload HTTP client")?;

        let resp = upload_client
            .put(url)
            .header("Content-Type", content_type)
            .body(data)
            .send()
            .await
            .context("Failed to upload to presigned URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Upload failed: {} — {}", status, text));
        }

        Ok(())
    }

    pub async fn upload_part_to_presigned_url(
        &self,
        url: &str,
        data: Vec<u8>,
        _size_bytes: u64,
    ) -> Result<String> {
        // Generous timeout: assume worst-case 1 MB/s, minimum 600s (10 min)
        let data_len = data.len() as u64;
        let calculated_secs = (data_len / (1024 * 1024)).saturating_add(60);
        let timeout_secs = std::cmp::max(600, calculated_secs);

        let upload_client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to build upload HTTP client")?;

        // Retry up to 3 times on transient failures
        let max_retries = 3;
        let mut last_err = None;
        for attempt in 1..=max_retries {
            let resp = upload_client.put(url).body(data.clone()).send().await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let etag = r
                        .headers()
                        .get("ETag")
                        .and_then(|v| v.to_str().ok())
                        .ok_or_else(|| anyhow!("ETag header missing in upload part response"))?
                        .to_string();
                    return Ok(etag);
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    last_err = Some(anyhow!("Upload part failed: {} — {}", status, text));
                }
                Err(e) => {
                    last_err = Some(anyhow!("Upload part network error: {}", e));
                }
            }

            if attempt < max_retries {
                eprintln!(
                    "Upload part attempt {}/{} failed, retrying in {}s...",
                    attempt,
                    max_retries,
                    attempt * 2
                );
                tokio::time::sleep(std::time::Duration::from_secs((attempt * 2) as u64)).await;
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("Upload part failed after {} retries", max_retries)))
    }

    // ── Backup Manifest ───────────────────────────────────────────────

    // ── Download from presigned URL ─────────────────────────────────

    pub async fn download_from_presigned_url(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .transfer_client
            .get(url)
            .send()
            .await
            .context("Failed to download from presigned URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Download failed: {} — {}", status, text));
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .context("Failed to read download body")
    }

    pub async fn download_stream_from_presigned_url(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self
            .transfer_client
            .get(url)
            .send()
            .await
            .context("Failed to download stream from presigned URL")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Stream download failed: {} — {}", status, text));
        }

        Ok(resp)
    }

    // ── Master Key ──────────────────────────────────────────────────

    /// Upload the encrypted master key to the server for safekeeping.
    pub async fn save_master_key(&self, encrypted_key: &str) -> Result<()> {
        let url = format!("{}/auth/master-key", self.base_url);
        let auth = self.auth_header()?;
        let body = serde_json::json!({ "encryptedMasterKey": encrypted_key });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to save master key")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Save master key failed: {}", text));
        }
        Ok(())
    }

    /// Retrieve the encrypted master key from the server.
    pub async fn get_master_key(&self) -> Result<MasterKeyResponse> {
        let url = format!("{}/auth/master-key", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to get master key")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Get master key failed: {}", text));
        }
        resp.json()
            .await
            .context("Failed to parse master key response")
    }

    /// Initialize the client-owned, versioned key-slot envelope. The server
    /// stores only opaque slot ciphertext plus a hash of the AMK verifier.
    pub async fn initialize_master_key_envelope(
        &self,
        current_password: &str,
        envelope: &str,
        verifier: &str,
    ) -> Result<MasterKeyEnvelopeWriteResponse> {
        let url = format!("{}/auth/master-key-envelope", self.base_url);
        let auth = self.auth_header()?;
        let envelope_value: serde_json::Value =
            serde_json::from_str(envelope).context("Invalid master-key envelope JSON")?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({
                "currentPassword": current_password,
                "verifier": verifier,
                "envelope": envelope_value,
            }))
            .send()
            .await
            .context("Failed to initialize the vault recovery envelope")?;
        parse_api_json(resp, "Initialize vault recovery envelope").await
    }

    /// Rotate the password slot only after proving possession of the AMK.
    pub async fn rotate_master_key_envelope(
        &self,
        expected_revision: u64,
        envelope: &str,
        verifier: &str,
    ) -> Result<MasterKeyEnvelopeWriteResponse> {
        let url = format!("{}/auth/master-key-envelope", self.base_url);
        let auth = self.auth_header()?;
        let envelope_value: serde_json::Value =
            serde_json::from_str(envelope).context("Invalid master-key envelope JSON")?;
        let resp = self
            .client
            .put(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({
                "expectedRevision": expected_revision,
                "verifier": verifier,
                "envelope": envelope_value,
            }))
            .send()
            .await
            .context("Failed to rotate the vault password slot")?;
        parse_api_json(resp, "Rotate vault password slot").await
    }

    pub async fn get_master_key_envelope(&self) -> Result<MasterKeyEnvelopeResponse> {
        let url = format!("{}/auth/master-key-envelope", self.base_url);
        let auth = self.auth_header()?;
        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to get the vault recovery envelope")?;
        parse_api_json(resp, "Get vault recovery envelope").await
    }

    // ── User Settings ───────────────────────────────────────────────

    /// Persist notification / webhook settings server-side.
    pub async fn save_settings(&self, settings: &UserSettings) -> Result<()> {
        let url = format!("{}/account/settings", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .put(&url)
            .header("Authorization", &auth)
            .json(settings)
            .send()
            .await
            .context("Failed to save settings")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Save settings failed: {}", text));
        }
        Ok(())
    }

    /// Fetch the user's settings from the server.
    pub async fn get_settings(&self) -> Result<UserSettings> {
        let url = format!("{}/account/settings", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to get settings")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Get settings failed: {}", text));
        }
        resp.json()
            .await
            .context("Failed to parse settings response")
    }

    // ── Notifications ───────────────────────────────────────────────

    /// Send a notification via the API. Returns the API response for debugging.
    pub async fn send_notification(
        &self,
        notification: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/notifications/send", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(notification)
            // A completed backup or restore must not remain blocked behind
            // Discord or notification infrastructure. The general API client
            // has a much longer transfer timeout, so bound this best-effort
            // request explicitly.
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
            .context("Failed to send notification")?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({"error": "Could not parse response"}));

        if !status.is_success() {
            return Err(anyhow!("Notification API returned {}: {}", status, body));
        }

        Ok(body)
    }

    // ── Backup Manifest ─────────────────────────────────────────────

    /// Fetch the manifest JSON for a given backup key.
    pub async fn get_manifest(&self, key: &str) -> Result<serde_json::Value> {
        let url = format!("{}/backup/manifest", self.base_url);
        let auth = self.auth_header()?;
        let body = serde_json::json!({ "key": key });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to get manifest")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Get manifest failed: {}", text));
        }
        resp.json().await.context("Failed to parse manifest")
    }

    // ── Folder Operations ───────────────────────────────────────────

    /// Create a new folder for organizing backups.
    pub async fn create_folder(
        &self,
        name: &str,
        parent_folder: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/backup/create-folder", self.base_url);
        let auth = self.auth_header()?;
        let body = serde_json::json!({
            "name": name,
            "parentFolder": parent_folder,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to create folder")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Create folder failed: {} — {}", status, text));
        }
        resp.json()
            .await
            .context("Failed to parse create folder response")
    }

    /// Create or rename the logical folder owned by one backup profile.
    pub async fn ensure_profile_folder(
        &self,
        profile_id: &str,
        profile_name: &str,
    ) -> Result<String> {
        let url = format!("{}/backup/profile-folders", self.base_url);
        let auth = self.auth_header()?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&serde_json::json!({
                "profileId": profile_id,
                "profileName": profile_name,
            }))
            .send()
            .await
            .context("Failed to organize the profile folder")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Profile folder setup failed: {} — {}",
                status,
                text
            ));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse profile folder response")?;
        value
            .get("folder")
            .and_then(serde_json::Value::as_str)
            .filter(|folder| !folder.is_empty())
            .map(ToString::to_string)
            .context("Profile folder response did not include a folder")
    }

    /// Stop treating a folder as profile-managed while preserving its backups.
    pub async fn detach_profile_folder(&self, profile_id: &str) -> Result<()> {
        let url = format!("{}/backup/profile-folders/{}", self.base_url, profile_id);
        let auth = self.auth_header()?;
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to detach the profile folder")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Profile folder detach failed: {} — {}",
                status,
                text
            ));
        }
        Ok(())
    }

    /// Move a backup to a different folder.
    pub async fn move_backup(
        &self,
        key: &str,
        destination_folder: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/backup/move", self.base_url);
        let auth = self.auth_header()?;
        let body = serde_json::json!({
            "key": key,
            "destinationFolder": destination_folder,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .json(&body)
            .send()
            .await
            .context("Failed to move backup")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Move backup failed: {} — {}", status, text));
        }
        resp.json()
            .await
            .context("Failed to parse move backup response")
    }

    /// Delete a folder.
    pub async fn delete_folder(&self, path: &str) -> Result<()> {
        let mut url = reqwest::Url::parse(&format!("{}/backup/folders", self.base_url))
            .context("Invalid folder API URL")?;
        url.query_pairs_mut().append_pair("path", path);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .delete(url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to delete folder")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Delete folder failed: {} — {}", status, text));
        }
        Ok(())
    }

    /// List logical folders. Repository objects are never exposed as folders.
    pub async fn list_folders(&self) -> Result<serde_json::Value> {
        let url = format!("{}/backup/folders", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to list folders")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("List folders failed: {} — {}", status, text));
        }
        resp.json()
            .await
            .context("Failed to parse folder list response")
    }

    pub async fn upload_kopia_manifest(&self, manifest_json: &str) -> Result<()> {
        let url = format!("{}/backup/kopia-manifest", self.base_url);
        let auth = self.auth_header()?;

        let resp = self
            .client
            .post(&url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .body(manifest_json.to_string())
            .send()
            .await
            .context("Failed to upload kopia manifest")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Upload manifest failed: {} — {}", status, text));
        }
        Ok(())
    }

    pub async fn get_kopia_manifest(&self) -> Result<serde_json::Value> {
        let url = format!("{}/backup/kopia-manifest", self.base_url);
        let auth = self.auth_header()?;
        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await
            .context("Failed to fetch kopia manifest")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Fetch manifest failed: {} — {}", status, text));
        }
        resp.json().await.context("Failed to parse kopia manifest")
    }
}

#[cfg(test)]
mod account_recovery_contract_tests {
    use super::{LoginResponse, MasterKeyEnvelopeResponse};

    #[test]
    fn login_response_matches_account_recovery_api_contract() {
        let parsed: LoginResponse = serde_json::from_value(serde_json::json!({
            "token": "jwt",
            "email": "owner@example.com",
            "encryptedMasterKey": "legacy:cipher:text",
            "encryptedMasterKeyEnvelope": "{\"version\":1}",
            "masterKeyRequiresOriginalPassword": true
        }))
        .unwrap();
        assert_eq!(
            parsed.encrypted_master_key.as_deref(),
            Some("legacy:cipher:text")
        );
        assert_eq!(
            parsed.encrypted_master_key_envelope.as_deref(),
            Some("{\"version\":1}")
        );
        assert!(parsed.master_key_requires_original_password);
    }

    #[test]
    fn envelope_get_response_keeps_legacy_and_account_recovery_boundary() {
        let parsed: MasterKeyEnvelopeResponse = serde_json::from_value(serde_json::json!({
            "envelope": "{\"version\":1}",
            "version": 1,
            "revision": 4,
            "legacyEncryptedMasterKey": "legacy",
            "accountRecoveryChangedPassword": true
        }))
        .unwrap();
        assert_eq!(parsed.revision, Some(4));
        assert_eq!(
            parsed.legacy_encrypted_master_key.as_deref(),
            Some("legacy")
        );
        assert!(parsed.account_recovery_changed_password);
    }
}

/// RAII lifecycle reporter: unfinished operations are recorded as failed when
/// their function returns early. All network delivery remains best-effort and
/// detached from the customer operation.
pub struct EngineJobReporter {
    api: SaveStateClient,
    id: String,
    kind: &'static str,
    trigger: &'static str,
    stage: &'static str,
    heartbeat_stage: Arc<Mutex<&'static str>>,
    heartbeat_task: Option<tokio::task::JoinHandle<()>>,
    started_at: String,
    organization_account_scope: Option<String>,
    finished: bool,
}

impl EngineJobReporter {
    pub fn start(
        api: SaveStateClient,
        id: String,
        kind: &'static str,
        trigger: &'static str,
    ) -> Self {
        Self::start_with_organization_scope(api, id, kind, trigger, None)
    }

    pub fn start_backup(
        api: SaveStateClient,
        id: String,
        trigger: &'static str,
        account_scope: String,
    ) -> Self {
        Self::start_with_organization_scope(api, id, "backup", trigger, Some(account_scope))
    }

    fn start_with_organization_scope(
        api: SaveStateClient,
        id: String,
        kind: &'static str,
        trigger: &'static str,
        organization_account_scope: Option<String>,
    ) -> Self {
        let started_at = chrono::Utc::now().to_rfc3339();
        api.report_job_event(
            &id,
            kind,
            "running",
            Some("starting"),
            Some(trigger),
            None,
            None,
            None,
            None,
            &started_at,
        );
        let heartbeat_stage = Arc::new(Mutex::new("starting"));
        let heartbeat_api = api.clone();
        let heartbeat_id = id.clone();
        let heartbeat_started_at = started_at.clone();
        let heartbeat_stage_task = Arc::clone(&heartbeat_stage);
        let heartbeat_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let stage = heartbeat_stage_task
                    .lock()
                    .map(|value| *value)
                    .unwrap_or("running");
                heartbeat_api.report_job_event(
                    &heartbeat_id,
                    kind,
                    "running",
                    Some(stage),
                    Some(trigger),
                    None,
                    None,
                    None,
                    None,
                    &heartbeat_started_at,
                );
            }
        });
        Self {
            api,
            id,
            kind,
            trigger,
            stage: "starting",
            heartbeat_stage,
            heartbeat_task: Some(heartbeat_task),
            started_at,
            organization_account_scope,
            finished: false,
        }
    }

    pub fn progress(&mut self, stage: &'static str) {
        if self.finished {
            return;
        }
        self.stage = stage;
        if let Ok(mut current) = self.heartbeat_stage.lock() {
            *current = stage;
        }
        self.api.report_job_event(
            &self.id,
            self.kind,
            "running",
            Some(stage),
            Some(self.trigger),
            None,
            None,
            None,
            None,
            &self.started_at,
        );
    }

    pub fn finish(
        &mut self,
        status: &'static str,
        stage: &'static str,
        bytes: Option<u64>,
        files: Option<u64>,
        error_code: Option<&'static str>,
    ) {
        self.finish_with_message(status, stage, bytes, files, error_code, None);
    }

    fn finish_with_message(
        &mut self,
        status: &'static str,
        stage: &'static str,
        bytes: Option<u64>,
        files: Option<u64>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) {
        if self.finished {
            return;
        }
        if let Some(task) = self.heartbeat_task.take() {
            task.abort();
        }
        self.stage = stage;
        self.api.report_job_event(
            &self.id,
            self.kind,
            status,
            Some(stage),
            Some(self.trigger),
            bytes,
            files,
            error_code,
            error_message,
            &self.started_at,
        );
        if self.kind == "backup" && matches!(status, "succeeded" | "failed") {
            if let Some(account_scope) = self.organization_account_scope.as_deref() {
                crate::organization_enrollment::queue_organization_installation_backup_heartbeat(
                    &self.api,
                    account_scope,
                    OrganizationBackupHeartbeat {
                        event_id: self.id.clone(),
                        status: status.to_string(),
                        occurred_at: chrono::Utc::now().to_rfc3339(),
                        error_code: error_code.map(str::to_string),
                        error_reason: error_message.map(str::to_string),
                    },
                );
            }
        }
        self.finished = true;
    }

    pub fn fail(&mut self, error_code: &'static str, bytes: Option<u64>, files: Option<u64>) {
        let stage = self.stage;
        let operation = match self.kind {
            "backup" => "Backup",
            "restore" => "Restore",
            "delete" => "Backup deletion",
            "maintenance" => "Repository maintenance",
            _ => "Operation",
        };
        let phase = match stage {
            "repository_connect" => "connecting to the encrypted repository",
            "snapshot_create" => "creating the encrypted snapshot",
            "snapshot_delete" => "deleting the snapshot",
            "manifest_lookup" => "loading snapshot metadata",
            "authorization" => "authorizing the restore",
            "snapshot_restore" => "restoring the snapshot",
            "manifest_sync" => "synchronizing backup metadata",
            "maintenance_run" => "running repository maintenance",
            _ => "startup",
        };
        let message = format!("{} failed while {}.", operation, phase);
        self.finish_with_message(
            "failed",
            stage,
            bytes,
            files,
            Some(error_code),
            Some(&message),
        );
    }
}

impl Drop for EngineJobReporter {
    fn drop(&mut self) {
        if !self.finished {
            self.fail("operation_failed", None, None);
        }
    }
}
