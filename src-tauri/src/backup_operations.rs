use crate::api::SaveStateClient;
use crate::state::{AppState, AppStateWrapper};
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone)]
pub struct AccountContext {
    pub api: SaveStateClient,
    pub account_scope: String,
    pub repository_password: String,
    pub session_generation: u64,
}

impl AccountContext {
    pub fn ensure_current(&self, state: &AppStateWrapper) -> Result<()> {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
        if !context_matches_session(
            self,
            guard.account_email().as_deref(),
            guard.session_generation,
            guard.master_key.is_some(),
        ) {
            return Err(anyhow!(
                "The signed-in account changed before the operation started"
            ));
        }
        Ok(())
    }

    pub fn capture(state: &AppState) -> Result<Self> {
        let account_scope = state
            .account_scope()
            .ok_or_else(|| anyhow!("Sign in before starting a backup"))?;
        let master_key = state
            .master_key
            .ok_or_else(|| anyhow!("Backup encryption key is unavailable"))?;
        Ok(Self {
            api: state.api.clone(),
            account_scope,
            repository_password: hex::encode(master_key),
            session_generation: state.session_generation,
        })
    }
}

#[derive(Default)]
struct ControlState {
    terminal: bool,
}

pub struct BackupControl {
    id: String,
    name: Mutex<String>,
    cancel_requested: AtomicBool,
    committed: AtomicBool,
    cancellation_error: Mutex<Option<String>>,
    state: tokio::sync::Mutex<ControlState>,
    cancel_notify: tokio::sync::Notify,
    terminal_notify: tokio::sync::Notify,
}

impl BackupControl {
    #[cfg(test)]
    pub(crate) fn fixture() -> Arc<Self> {
        Arc::new(Self::new(
            "disposable-fixture".into(),
            "Local integration test".into(),
        ))
    }

    #[cfg(test)]
    pub(crate) async fn cancel_fixture(&self) {
        self.request_cancel().await;
    }

    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name: Mutex::new(name),
            cancel_requested: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            cancellation_error: Mutex::new(None),
            state: tokio::sync::Mutex::new(ControlState::default()),
            cancel_notify: tokio::sync::Notify::new(),
            terminal_notify: tokio::sync::Notify::new(),
        }
    }

    fn name(&self) -> String {
        self.name
            .lock()
            .map(|name| name.clone())
            .unwrap_or_else(|_| "Backup".to_string())
    }

    pub fn set_name(&self, name: impl Into<String>) {
        if let Ok(mut current) = self.name.lock() {
            *current = name.into();
        }
    }

    pub fn is_cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    async fn request_cancel(&self) -> bool {
        let state = self.state.lock().await;
        if state.terminal || self.is_committed() || self.is_cancel_requested() {
            return false;
        }
        self.cancel_requested.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        true
    }

    pub async fn wait_cancelled(&self) {
        loop {
            let notified = self.cancel_notify.notified();
            if self.is_cancel_requested() {
                return;
            }
            notified.await;
        }
    }

    pub async fn mark_committed(&self) -> Result<()> {
        let _state = self.state.lock().await;
        if self.is_cancel_requested() {
            return Err(cancelled_error());
        }
        self.committed.store(true, Ordering::Release);
        Ok(())
    }

    fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }

    pub fn record_cancellation_error(&self, error: impl Into<String>) {
        if let Ok(mut current) = self.cancellation_error.lock() {
            *current = Some(error.into());
        }
    }

    async fn finish(&self) {
        self.state.lock().await.terminal = true;
        self.terminal_notify.notify_waiters();
    }

    async fn wait_terminal(&self) {
        loop {
            let notified = self.terminal_notify.notified();
            if self.state.lock().await.terminal {
                return;
            }
            notified.await;
        }
    }

    fn cancellation_error(&self) -> Option<String> {
        self.cancellation_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }
}

#[derive(Default)]
struct RegistryState {
    active: HashMap<String, Arc<BackupControl>>,
    pending_logout: Option<String>,
    session_transition: bool,
}

#[derive(Default)]
struct Registry {
    state: Mutex<RegistryState>,
}

impl Registry {
    fn register(&self, name: String) -> Result<Arc<BackupControl>> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("Backup registry lock: {}", error))?;
        if state.pending_logout.is_some() || state.session_transition {
            return Err(anyhow!(
                "The account is changing or signing out; wait before starting another backup"
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let control = Arc::new(BackupControl::new(id.clone(), name));
        state.active.insert(id, Arc::clone(&control));
        Ok(control)
    }

    fn unregister(&self, id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.active.remove(id);
        }
    }

    fn prepare_logout(&self) -> Result<PreparedLogout> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("Backup registry lock: {}", error))?;
        if state.pending_logout.is_some() || state.session_transition {
            return Err(anyhow!("Sign-out is already in progress"));
        }
        let token = uuid::Uuid::new_v4().to_string();
        state.pending_logout = Some(token.clone());
        let mut active_backups: Vec<_> = state
            .active
            .values()
            .filter(|control| !control.is_committed())
            .map(|control| ActiveBackupSummary {
                id: control.id.clone(),
                name: control.name(),
            })
            .collect();
        active_backups.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(PreparedLogout {
            token,
            active_backups,
        })
    }

    fn controls_for_logout(&self, token: &str) -> Result<Vec<Arc<BackupControl>>> {
        let state = self
            .state
            .lock()
            .map_err(|error| anyhow!("Backup registry lock: {}", error))?;
        if state.pending_logout.as_deref() != Some(token) {
            return Err(anyhow!("Sign-out confirmation expired; try again"));
        }
        Ok(state
            .active
            .values()
            .filter(|control| !control.is_committed())
            .cloned()
            .collect())
    }

    fn finish_logout(&self, token: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state.pending_logout.as_deref() == Some(token) {
                state.pending_logout = None;
            }
        }
    }
}

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry() -> Arc<Registry> {
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(Registry::default())))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveBackupSummary {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedLogout {
    pub token: String,
    pub active_backups: Vec<ActiveBackupSummary>,
}

pub struct BackupOperation {
    pub context: AccountContext,
    pub control: Arc<BackupControl>,
    registry: Arc<Registry>,
}

impl BackupOperation {
    pub fn account_scope(&self) -> &str {
        &self.context.account_scope
    }

    pub fn api(&self) -> &SaveStateClient {
        &self.context.api
    }

    pub fn set_name(&self, name: impl Into<String>) {
        self.control.set_name(name);
    }

    pub fn ensure_not_cancelled(&self) -> Result<()> {
        if self.control.is_cancel_requested() {
            Err(cancelled_error())
        } else {
            Ok(())
        }
    }

    pub async fn finish_tracking(&self) {
        self.control.finish().await;
    }
}

impl Drop for BackupOperation {
    fn drop(&mut self) {
        self.registry.unregister(&self.control.id);
        let control = Arc::clone(&self.control);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                control.finish().await;
            });
        }
    }
}

pub struct LogoutGuard {
    registry: Arc<Registry>,
    token: String,
    pub cancelled_count: usize,
}

impl Drop for LogoutGuard {
    fn drop(&mut self) {
        self.registry.finish_logout(&self.token);
    }
}

pub fn begin(state: &AppStateWrapper, name: impl Into<String>) -> Result<BackupOperation> {
    let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
    let context = AccountContext::capture(&guard)?;
    drop(guard);
    begin_with_context(state, context, name)
}

/// Start an operation for a specific account workspace without changing the
/// workspace selected in the UI. The scheduler uses this to keep every
/// workspace's profiles running while one shared login remains active.
pub fn begin_with_context(
    state: &AppStateWrapper,
    context: AccountContext,
    name: impl Into<String>,
) -> Result<BackupOperation> {
    let guard = state.0.lock().map_err(|error| anyhow!("Lock: {}", error))?;
    let account_email = guard.account_email();
    if !context_matches_session(
        &context,
        account_email.as_deref(),
        guard.session_generation,
        guard.master_key.is_some(),
    ) {
        return Err(anyhow!(
            "The signed-in account changed before the backup started"
        ));
    }
    let registry = registry();
    let control = registry.register(name.into())?;
    drop(guard);
    Ok(BackupOperation {
        context,
        control,
        registry,
    })
}

fn context_matches_session(
    context: &AccountContext,
    account_email: Option<&str>,
    session_generation: u64,
    vault_unlocked: bool,
) -> bool {
    let expected_prefix =
        account_email.map(|email| format!("{}::", email.trim().to_ascii_lowercase()));
    vault_unlocked
        && session_generation == context.session_generation
        && expected_prefix
            .as_deref()
            .is_some_and(|prefix| context.account_scope.starts_with(prefix))
}

/// Hold both admission barriers across the entire account/workspace change,
/// including API awaits. Lock acquisition never waits while holding registry state.
pub struct SessionChangeGuard<'a> {
    registry: Arc<Registry>,
    _engine: tokio::sync::RwLockWriteGuard<'a, ()>,
}

impl Drop for SessionChangeGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.registry.state.lock() {
            state.session_transition = false;
        }
    }
}

fn begin_session_change_in<'a>(
    registry: Arc<Registry>,
    acquire_engine: impl FnOnce() -> Result<tokio::sync::RwLockWriteGuard<'a, ()>>,
) -> Result<SessionChangeGuard<'a>> {
    {
        let mut state = registry
            .state
            .lock()
            .map_err(|error| anyhow!("Backup registry lock: {error}"))?;
        if state.pending_logout.is_some() || state.session_transition || !state.active.is_empty() {
            return Err(anyhow!(
                "Wait for active operations or sign-out before changing accounts or workspaces"
            ));
        }
        state.session_transition = true;
    }
    match acquire_engine() {
        Ok(engine) => Ok(SessionChangeGuard {
            registry,
            _engine: engine,
        }),
        Err(error) => {
            if let Ok(mut state) = registry.state.lock() {
                state.session_transition = false;
            }
            Err(error)
        }
    }
}

pub fn begin_session_change() -> Result<SessionChangeGuard<'static>> {
    begin_session_change_in(registry(), crate::kopia::try_begin_update)
}

pub fn prepare_logout() -> Result<PreparedLogout> {
    registry().prepare_logout()
}

pub fn abort_logout(token: &str) -> Result<()> {
    let registry = registry();
    let valid = registry
        .state
        .lock()
        .map_err(|error| anyhow!("Backup registry lock: {}", error))?
        .pending_logout
        .as_deref()
        == Some(token);
    if !valid {
        return Err(anyhow!("Sign-out confirmation expired; try again"));
    }
    registry.finish_logout(token);
    Ok(())
}

pub async fn stop_for_logout(token: &str) -> Result<LogoutGuard> {
    stop_for_logout_in(registry(), token).await
}

async fn stop_for_logout_in(registry: Arc<Registry>, token: &str) -> Result<LogoutGuard> {
    let controls = registry.controls_for_logout(token)?;
    let mut requested = 0;
    for control in &controls {
        if control.request_cancel().await {
            requested += 1;
        }
    }
    for control in &controls {
        control.wait_terminal().await;
    }

    let failures = cancellation_failures(&controls);
    if !failures.is_empty() {
        registry.finish_logout(token);
        return Err(anyhow!(
            "Could not safely stop every backup. You are still signed in. {}",
            failures.join("; ")
        ));
    }

    Ok(LogoutGuard {
        registry,
        token: token.to_string(),
        cancelled_count: requested,
    })
}

fn cancellation_failures(controls: &[Arc<BackupControl>]) -> Vec<String> {
    controls
        .iter()
        .filter_map(|control| {
            control
                .cancellation_error()
                .map(|error| format!("{}: {}", control.name(), error))
        })
        .collect()
}

pub fn cancelled_error() -> anyhow::Error {
    anyhow!("BACKUP_CANCELLED: Backup stopped because the user signed out")
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.to_string().contains("BACKUP_CANCELLED")
}

#[cfg(test)]
mod tests {
    use super::begin_session_change_in;
    use super::{
        cancellation_failures, context_matches_session, stop_for_logout_in, AccountContext,
        Registry,
    };
    use crate::api::SaveStateClient;
    use std::sync::Arc;

    #[test]
    fn delayed_operation_rejects_replaced_or_signed_out_sessions() {
        use crate::state::{AppState, AppStateWrapper};
        use base64::Engine;
        let mut state = AppState::new(
            SaveStateClient::new("offline-fixture".into()),
            rusqlite::Connection::open_in_memory().unwrap(),
        );
        let claims =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"serviceId":12}"#);
        state.api.set_token(format!("header.{claims}.signature"));
        state.email = Some("fixture@example.invalid".into());
        state.master_key = Some([0; 32]);
        let context = AccountContext::capture(&state).unwrap();
        let state = AppStateWrapper(std::sync::Mutex::new(state));
        assert!(context.ensure_current(&state).is_ok());
        state.0.lock().unwrap().session_generation += 1;
        assert!(context.ensure_current(&state).is_err());
        state.0.lock().unwrap().session_generation -= 1;
        state.0.lock().unwrap().api.clear_token();
        assert!(context.ensure_current(&state).is_err());
    }

    #[tokio::test]
    async fn workspace_transition_excludes_backup_restore_and_concurrent_session_changes() {
        let registry = Arc::new(Registry::default());
        let engine = tokio::sync::RwLock::new(());
        let acquire = || {
            engine
                .try_write()
                .map_err(|_| anyhow::anyhow!("engine busy"))
        };
        let restore = engine.read().await;
        assert!(begin_session_change_in(Arc::clone(&registry), acquire).is_err());
        // A failed transition must not poison admission.
        let backup = registry.register("backup".into()).unwrap();
        drop(restore);
        assert!(begin_session_change_in(Arc::clone(&registry), acquire).is_err());
        registry.unregister(&backup.id);
        let transition = begin_session_change_in(Arc::clone(&registry), acquire).unwrap();
        tokio::task::yield_now().await; // Represents an in-flight workspace API request.
        assert!(registry.register("late backup".into()).is_err());
        assert!(registry.prepare_logout().is_err());
        assert!(engine.try_read().is_err());
        assert!(begin_session_change_in(Arc::clone(&registry), acquire).is_err());
        drop(transition);
        assert!(engine.try_read().is_ok());
        assert!(registry.register("new account backup".into()).is_ok());
    }

    #[test]
    fn inactive_workspace_contexts_are_valid_only_for_the_same_login_session() {
        let context = AccountContext {
            api: SaveStateClient::new("test-installation".into()),
            account_scope: "owner@example.com::service:22".into(),
            repository_password: "secret".into(),
            session_generation: 7,
        };
        assert!(context_matches_session(
            &context,
            Some("owner@example.com"),
            7,
            true
        ));
        assert!(!context_matches_session(
            &context,
            Some("other@example.com"),
            7,
            true
        ));
        assert!(!context_matches_session(
            &context,
            Some("owner@example.com"),
            8,
            true
        ));
        assert!(!context_matches_session(
            &context,
            Some("owner@example.com"),
            7,
            false
        ));
    }

    #[tokio::test]
    async fn logout_cancels_all_registered_backups_and_blocks_new_work() {
        let registry = Registry::default();
        let quick = registry.register("Pictures".into()).unwrap();
        let scheduled = registry.register("Nightly database".into()).unwrap();
        let prepared = registry.prepare_logout().unwrap();
        assert_eq!(prepared.active_backups.len(), 2);
        let controls = registry.controls_for_logout(&prepared.token).unwrap();
        assert_eq!(controls.len(), 2);
        assert!(registry.register("Too late".into()).is_err());
        assert!(quick.request_cancel().await);
        assert!(scheduled.request_cancel().await);
        quick.finish().await;
        scheduled.finish().await;
        quick.wait_terminal().await;
        scheduled.wait_terminal().await;
        registry.finish_logout(&prepared.token);
        assert!(registry.register("Allowed again".into()).is_ok());
    }

    #[test]
    fn declining_logout_clears_the_atomic_start_barrier() {
        let registry = Registry::default();
        let prepared = registry.prepare_logout().unwrap();
        assert!(registry.register("Blocked".into()).is_err());
        registry.finish_logout(&prepared.token);
        assert!(registry.register("Allowed".into()).is_ok());
    }

    #[tokio::test]
    async fn cancellation_is_idempotent() {
        let registry = Registry::default();
        let backup = registry.register("Archive".into()).unwrap();
        assert!(backup.request_cancel().await);
        assert!(!backup.request_cancel().await);
    }

    #[tokio::test]
    async fn completion_and_cancellation_have_one_atomic_winner() {
        let registry = Registry::default();
        let completed = registry.register("Completed".into()).unwrap();
        completed.mark_committed().await.unwrap();
        assert!(!completed.request_cancel().await);
        let cancelled = registry.register("Cancelled".into()).unwrap();
        assert!(cancelled.request_cancel().await);
        assert!(cancelled.mark_committed().await.is_err());
    }

    #[tokio::test]
    async fn cancellation_failure_is_retained_for_logout() {
        let registry = Registry::default();
        let backup = registry.register("Large backup".into()).unwrap();
        assert!(backup.request_cancel().await);
        backup.record_cancellation_error("Windows denied process termination");
        backup.finish().await;
        assert_eq!(
            backup.cancellation_error().as_deref(),
            Some("Windows denied process termination")
        );
        let failures = cancellation_failures(&[backup]);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Windows denied process termination"));
    }

    #[tokio::test]
    async fn cancellation_failure_clears_the_atomic_start_barrier() {
        let registry = Arc::new(Registry::default());
        let backup = registry.register("Large backup".into()).unwrap();
        let prepared = registry.prepare_logout().unwrap();
        let worker = tokio::spawn(async move {
            backup.wait_cancelled().await;
            backup.record_cancellation_error("Windows denied process termination");
            backup.finish().await;
        });

        let result = stop_for_logout_in(Arc::clone(&registry), &prepared.token).await;
        worker.await.unwrap();

        assert!(result.is_err());
        assert!(registry
            .register("Allowed after failed sign-out".into())
            .is_ok());
    }
}
