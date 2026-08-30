use crate::api::SaveStateClient;
use rusqlite::Connection;
use std::sync::Mutex;

/// Shared application state accessible from all Tauri commands.
///
/// `master_key` holds the 32-byte AES-256 key that is derived on login.
/// It replaces the old passphrase-based approach so encryption keys are
/// generated once, stored encrypted on the server, and decrypted locally
/// with the user's password.
pub struct AppState {
    pub api: SaveStateClient,
    pub db: Connection,
    pub email: Option<String>,
    pub master_key: Option<[u8; 32]>,
    pub session_generation: u64,
}

/// Wrapper to make AppState sendable via Tauri's state management.
pub struct AppStateWrapper(pub Mutex<AppState>);

impl AppState {
    pub fn new(api: SaveStateClient, db: Connection) -> Self {
        Self {
            api,
            db,
            email: None,
            master_key: None,
            session_generation: 0,
        }
    }

    /// Return the canonical local ownership scope for the active account.
    /// Profiles must never be read or mutated without this scope.
    pub fn account_scope(&self) -> Option<String> {
        if self.api.token.is_none() || self.master_key.is_none() {
            return None;
        }
        let email = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(str::to_ascii_lowercase)?;
        let workspace_id = self.api.workspace_id()?;
        Some(format!("{email}::{workspace_id}"))
    }

    pub fn account_email(&self) -> Option<String> {
        if self.api.token.is_none() || self.master_key.is_none() {
            return None;
        }
        self.email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(str::to_ascii_lowercase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    #[test]
    fn account_scope_requires_a_complete_authenticated_session() {
        let db = Connection::open_in_memory().unwrap();
        let mut state = AppState::new(SaveStateClient::new("test-installation".into()), db);
        state.email = Some(" Owner@Example.COM ".into());
        assert!(state.account_scope().is_none());

        let claims = URL_SAFE_NO_PAD.encode(br#"{"serviceId":12}"#);
        state.api.set_token(format!("header.{claims}.signature"));
        assert!(state.account_scope().is_none());

        state.master_key = Some([7; 32]);
        assert_eq!(
            state.account_scope().as_deref(),
            Some("owner@example.com::service:12")
        );
        assert_eq!(state.account_email().as_deref(), Some("owner@example.com"));
    }
}
