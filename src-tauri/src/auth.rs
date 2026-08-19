use crate::state::AppStateWrapper;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit};
use anyhow::{anyhow, Context, Result};
use argon2::Argon2;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ────────────────────────────────────────────────────────────────────
// Persisted session (OS credential vault, never a plaintext password)
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    email: String,
    token: String,
    master_key: String,
}

fn legacy_creds_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("SaveState").join("credentials.json")
}

fn credential_entry() -> Result<keyring::v1::Entry> {
    keyring::v1::Entry::new("SaveState Vault", "remembered-session")
        .context("Windows Credential Manager is unavailable")
}

fn save_session(email: &str, token: &str, master_key: &[u8; 32]) -> Result<()> {
    let session = StoredSession {
        email: email.to_string(),
        token: token.to_string(),
        master_key: hex::encode(master_key),
    };
    let json = serde_json::to_vec(&session)?;
    credential_entry()?
        .set_secret(&json)
        .context("Failed to save the remembered session securely")?;

    // Remove the old plaintext token file after a secure save succeeds.
    let _ = std::fs::remove_file(legacy_creds_path());
    Ok(())
}

fn load_session() -> Option<(StoredSession, [u8; 32])> {
    let data = credential_entry().ok()?.get_secret().ok()?;
    let session: StoredSession = serde_json::from_slice(&data).ok()?;
    let decoded = hex::decode(&session.master_key).ok()?;
    let master_key: [u8; 32] = decoded.try_into().ok()?;
    Some((session, master_key))
}

fn clear_persisted_session() {
    if let Ok(entry) = credential_entry() {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(legacy_creds_path());
}

// ────────────────────────────────────────────────────────────────────
// Master key crypto helpers
// ────────────────────────────────────────────────────────────────────

/// Derive a 256-bit wrapping key from password + salt using Argon2id.
fn derive_wrapping_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .expect("Argon2 key derivation failed");
    key
}

/// Encrypt the master key with a password-derived wrapping key.
/// Output format: `hex(salt):hex(nonce):hex(ciphertext+tag)`
fn encrypt_master_key(master_key: &[u8; 32], password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);

    let wrapping_key = derive_wrapping_key(password, &salt);
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&wrapping_key));

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = GenericArray::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, master_key.as_ref())
        .expect("Encryption failed");

    format!(
        "{}:{}:{}",
        hex::encode(salt),
        hex::encode(nonce_bytes),
        hex::encode(ciphertext)
    )
}

/// Decrypt the master key from the `salt:nonce:ciphertext` format.
fn decrypt_master_key(encrypted: &str, password: &str) -> Result<[u8; 32]> {
    let parts: Vec<&str> = encrypted.split(':').collect();
    if parts.len() != 3 {
        return Err(anyhow!("Invalid encrypted master key format"));
    }

    let salt = hex::decode(parts[0]).context("Bad salt hex")?;
    let nonce_bytes = hex::decode(parts[1]).context("Bad nonce hex")?;
    let ciphertext = hex::decode(parts[2]).context("Bad ciphertext hex")?;

    let wrapping_key = derive_wrapping_key(password, &salt);
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&wrapping_key));
    let nonce = GenericArray::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| anyhow!("Failed to decrypt master key — wrong password?"))?;

    if plaintext.len() != 32 {
        return Err(anyhow!(
            "Decrypted master key has wrong length: {}",
            plaintext.len()
        ));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&plaintext);
    Ok(key)
}

// ────────────────────────────────────────────────────────────────────
// Response types for the frontend
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub email: Option<String>,
    pub master_key_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResult {
    pub success: bool,
    pub email: String,
}

// ────────────────────────────────────────────────────────────────────
// Tauri commands
// ────────────────────────────────────────────────────────────────────

/// Login flow:
/// 1. Authenticate with the API (get token + optional encrypted master key)
/// 2. If encrypted_master_key is present → decrypt it with the password
/// 3. If absent (first login) → generate a fresh master key, encrypt, upload
/// 4. Store the decrypted master_key in AppState
#[tauri::command]
pub async fn cmd_login(
    state: tauri::State<'_, AppStateWrapper>,
    email: String,
    password: String,
    remember_me: Option<bool>,
) -> std::result::Result<LoginResult, String> {
    login_inner(&state, &email, &password, remember_me.unwrap_or(true))
        .await
        .map_err(|e| e.to_string())
}

async fn login_inner(
    state: &AppStateWrapper,
    email: &str,
    password: &str,
    remember_me: bool,
) -> Result<LoginResult> {
    // Clone the client to avoid holding the lock across await
    let mut api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock error: {}", e))?;
        guard.api.clone()
    };

    // 1. Authenticate
    let login_resp = api.login(email, password).await?;
    let account_email = login_resp
        .email
        .as_deref()
        .unwrap_or(email)
        .trim()
        .to_ascii_lowercase();

    // 2. Handle master key
    let master_key = if let Some(ref encrypted) = login_resp.encrypted_master_key {
        // Existing key on server — decrypt with password
        decrypt_master_key(encrypted, password)?
    } else {
        // First login — generate a new random master key
        let mut new_key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut new_key);

        // Encrypt it with the password and upload
        let encrypted = encrypt_master_key(&new_key, password);
        api.save_master_key(&encrypted).await?;

        new_key
    };

    // 3. Persist only when the user opts in. The token and decrypted master
    // key live in the native OS credential vault, not a plaintext JSON file.
    if remember_me {
        save_session(&account_email, &login_resp.token, &master_key)?;
    } else {
        clear_persisted_session();
    }

    // 4. Store token, email, and master key in memory.
    {
        let mut guard = state.0.lock().map_err(|e| anyhow!("Lock error: {}", e))?;
        guard.api.set_token(login_resp.token.clone());
        guard.email = Some(account_email.clone());
        guard.master_key = Some(master_key);
    }
    crate::kopia::clear_session_cache();

    Ok(LoginResult {
        success: true,
        email: account_email,
    })
}

#[tauri::command]
pub async fn cmd_logout(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<(), String> {
    {
        let mut guard = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
        guard.api.clear_token();
        guard.email = None;
        guard.master_key = None;
    }
    clear_persisted_session();
    crate::kopia::clear_session_cache();
    Ok(())
}

#[tauri::command]
pub async fn cmd_get_auth_status(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<AuthStatus, String> {
    let guard = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
    Ok(AuthStatus {
        authenticated: guard.api.token.is_some(),
        email: guard.email.clone(),
        master_key_ready: guard.master_key.is_some(),
    })
}

#[tauri::command]
pub async fn cmd_get_account(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
        guard.api.clone()
    };

    let account = api.get_account().await.map_err(|e| e.to_string())?;
    serde_json::to_value(&account).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_cancel_subscription(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
        guard.api.clone()
    };

    let resp = api.cancel_subscription().await.map_err(|e| e.to_string())?;
    serde_json::to_value(&resp).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_resume_subscription(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<serde_json::Value, String> {
    let api = {
        let guard = state.0.lock().map_err(|e| format!("Lock error: {}", e))?;
        guard.api.clone()
    };

    let resp = api.resume_subscription().await.map_err(|e| e.to_string())?;
    Ok(resp)
}

// ────────────────────────────────────────────────────────────────────
// Try to restore a previous session on startup
// ────────────────────────────────────────────────────────────────────

/// Restores the token, email, and decrypted master key from the OS credential
/// vault when the user selected Remember me. Legacy plaintext token files are
/// deliberately removed and never reused.
pub fn try_restore_session(state: &AppStateWrapper) {
    let _ = std::fs::remove_file(legacy_creds_path());
    if let Some((session, master_key)) = load_session() {
        if let Ok(mut guard) = state.0.lock() {
            guard.api.set_token(session.token);
            guard.email = Some(session.email.trim().to_ascii_lowercase());
            guard.master_key = Some(master_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{decrypt_master_key, encrypt_master_key};

    #[test]
    fn master_key_round_trips_through_password_wrapping() {
        let master_key = [0x5Au8; 32];
        let encrypted = encrypt_master_key(&master_key, "correct horse battery staple");
        let decrypted = decrypt_master_key(&encrypted, "correct horse battery staple").unwrap();
        assert_eq!(decrypted, master_key);
    }

    #[test]
    fn wrong_password_cannot_unwrap_master_key() {
        let encrypted = encrypt_master_key(&[0xA5u8; 32], "right password");
        assert!(decrypt_master_key(&encrypted, "wrong password").is_err());
    }
}
