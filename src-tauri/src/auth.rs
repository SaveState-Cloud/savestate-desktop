use crate::api::SaveStateClient;
use crate::state::AppStateWrapper;
use aes_gcm::{
    aead::{Aead, Key, Nonce, Payload},
    Aes256Gcm, KeyInit,
};
use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const ENVELOPE_VERSION: u32 = 1;
const VERIFIER_DOMAIN: &[u8] = b"savestate-envelope-verifier-v1";
const KEY_ID_DOMAIN: &[u8] = b"savestate-envelope-key-id-v1";
const OFFLINE_SLOT_DOMAIN: &[u8] = b"savestate-offline-vault-slot-v1";
const JSON_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;

// Persisted session (OS credential vault, never a plaintext password).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    email: String,
    token: String,
    master_key: String,
    #[serde(default)]
    key_id: Option<String>,
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
        key_id: Some(master_key_id(master_key)),
    };
    let json = serde_json::to_vec(&session)?;
    credential_entry()?
        .set_secret(&json)
        .context("Failed to save the remembered session securely")?;
    let _ = std::fs::remove_file(legacy_creds_path());
    Ok(())
}

fn load_session() -> Option<(StoredSession, [u8; 32])> {
    let data = credential_entry().ok()?.get_secret().ok()?;
    let session: StoredSession = serde_json::from_slice(&data).ok()?;
    let decoded = hex::decode(&session.master_key).ok()?;
    let master_key: [u8; 32] = decoded.try_into().ok()?;
    if session
        .key_id
        .as_deref()
        .is_some_and(|stored| stored != master_key_id(&master_key))
    {
        return None;
    }
    Some((session, master_key))
}

fn clear_persisted_session() {
    if let Ok(entry) = credential_entry() {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(legacy_creds_path());
}

// Legacy password wrapper (read compatibility and initialize-once fallback).
fn derive_wrapping_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .expect("Argon2 key derivation failed");
    key
}

fn password_slot_kdf_params() -> Params {
    Params::new(19_456, 2, 1, Some(32)).expect("fixed password-slot Argon2 profile is valid")
}

/// Stable, versioned KDF profile for envelope password slots. Do not replace
/// this with library defaults: those are not a durable encrypted-file format.
fn derive_password_slot_key(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        password_slot_kdf_params(),
    )
    .hash_password_into(password.as_bytes(), salt, &mut key)
    .expect("fixed password-slot Argon2 derivation failed");
    key
}

/// Legacy output format: `hex(salt):hex(nonce):hex(ciphertext+tag)`.
fn encrypt_master_key(master_key: &[u8; 32], password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let wrapping_key = derive_wrapping_key(password, &salt);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(wrapping_key));

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::<Aes256Gcm>::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, master_key.as_ref())
        .expect("Encryption failed");
    format!(
        "{}:{}:{}",
        hex::encode(salt),
        hex::encode(nonce_bytes),
        hex::encode(ciphertext)
    )
}

fn decrypt_master_key(encrypted: &str, password: &str) -> Result<[u8; 32]> {
    let parts: Vec<&str> = encrypted.split(':').collect();
    if parts.len() != 3 {
        bail!("Invalid legacy encrypted master key format");
    }
    let salt = hex::decode(parts[0]).context("Bad legacy salt")?;
    let nonce_bytes = hex::decode(parts[1]).context("Bad legacy nonce")?;
    if nonce_bytes.len() != 12 {
        bail!("Invalid legacy nonce length");
    }
    let ciphertext = hex::decode(parts[2]).context("Bad legacy ciphertext")?;
    let wrapping_key = derive_wrapping_key(password, &salt);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(wrapping_key));
    let nonce = Nonce::<Aes256Gcm>::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow!("Encrypted master key has an invalid nonce length"))?;

    let plaintext = cipher
        .decrypt(&nonce, ciphertext.as_ref())
        .map_err(|_| anyhow!("The vault password did not unlock this legacy vault"))?;

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

// Versioned client-owned key envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct MasterKeyEnvelope {
    version: u32,
    revision: u64,
    key_id: String,
    slots: Vec<MasterKeySlot>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct MasterKeySlot {
    id: String,
    #[serde(rename = "type")]
    slot_type: String,
    wrapped_key: String,
    nonce: String,
    kdf: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kdf_params: Option<PasswordSlotKdfParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    salt: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PasswordSlotKdfParams {
    algorithm: String,
    version: u32,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    output_bytes: u32,
}

fn fixed_password_slot_kdf_metadata() -> PasswordSlotKdfParams {
    PasswordSlotKdfParams {
        algorithm: "argon2id".into(),
        version: 19,
        memory_kib: 19_456,
        iterations: 2,
        parallelism: 1,
        output_bytes: 32,
    }
}

fn random_id() -> String {
    let mut value = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn master_key_id(master_key: &[u8; 32]) -> String {
    let mut digest = Sha256::new();
    digest.update(KEY_ID_DOMAIN);
    digest.update(master_key);
    URL_SAFE_NO_PAD.encode(&digest.finalize()[..18])
}

fn master_key_verifier(master_key: &[u8; 32]) -> String {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(master_key)
        .expect("HMAC accepts a 256-bit key");
    mac.update(VERIFIER_DOMAIN);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn offline_wrapping_key(recovery_key: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(recovery_key)
        .expect("HMAC accepts a 256-bit key");
    mac.update(OFFLINE_SLOT_DOMAIN);
    mac.finalize().into_bytes().into()
}

fn slot_aad(
    version: u32,
    key_id: &str,
    id: &str,
    slot_type: &str,
    kdf: &str,
    kdf_params: Option<&PasswordSlotKdfParams>,
    salt: Option<&str>,
) -> Vec<u8> {
    let params = kdf_params
        .map(|value| serde_json::to_string(value).expect("KDF metadata serializes"))
        .unwrap_or_default();
    format!(
        "savestate-envelope-slot-v1\n{version}\n{key_id}\n{id}\n{slot_type}\n{kdf}\n{params}\n{}",
        salt.unwrap_or("")
    )
    .into_bytes()
}

fn wrap_slot(
    master_key: &[u8; 32],
    wrapping_key: &[u8; 32],
    key_id: &str,
    slot_type: &str,
    kdf: &str,
    kdf_params: Option<PasswordSlotKdfParams>,
    salt: Option<String>,
) -> MasterKeySlot {
    let id = random_id();
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*wrapping_key));
    let aad = slot_aad(
        ENVELOPE_VERSION,
        key_id,
        &id,
        slot_type,
        kdf,
        kdf_params.as_ref(),
        salt.as_deref(),
    );
    let nonce_value = Nonce::<Aes256Gcm>::from(nonce);
    let ciphertext = cipher
        .encrypt(
            &nonce_value,
            Payload {
                msg: master_key,
                aad: &aad,
            },
        )
        .expect("AMK slot encryption failed");
    MasterKeySlot {
        id,
        slot_type: slot_type.to_string(),
        wrapped_key: URL_SAFE_NO_PAD.encode(ciphertext),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        kdf: kdf.to_string(),
        kdf_params,
        salt,
        extra: BTreeMap::new(),
    }
}

fn unwrap_slot(
    envelope: &MasterKeyEnvelope,
    slot: &MasterKeySlot,
    key: &[u8; 32],
) -> Result<[u8; 32]> {
    validate_envelope(envelope)?;
    let nonce = URL_SAFE_NO_PAD
        .decode(&slot.nonce)
        .context("Vault slot nonce is not valid base64url")?;
    if nonce.len() != 12 {
        bail!("Vault slot nonce has an invalid length");
    }
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&slot.wrapped_key)
        .context("Vault slot ciphertext is not valid base64url")?;
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let nonce_value = Nonce::<Aes256Gcm>::try_from(nonce.as_slice())
        .map_err(|_| anyhow!("Vault slot nonce has an invalid length"))?;
    let aad = slot_aad(
        envelope.version,
        &envelope.key_id,
        &slot.id,
        &slot.slot_type,
        &slot.kdf,
        slot.kdf_params.as_ref(),
        slot.salt.as_deref(),
    );
    let plaintext = cipher
        .decrypt(
            &nonce_value,
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow!("That vault unlock factor was not accepted"))?;
    let master_key: [u8; 32] = plaintext
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("Unwrapped master key has wrong length: {}", v.len()))?;
    if master_key_id(&master_key) != envelope.key_id {
        bail!("Vault key ID mismatch; the envelope may have been changed");
    }
    Ok(master_key)
}

fn password_slot(master_key: &[u8; 32], password: &str, key_id: &str) -> MasterKeySlot {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let wrapping_key = derive_password_slot_key(password, &salt);
    wrap_slot(
        master_key,
        &wrapping_key,
        key_id,
        "password",
        "argon2id-v1",
        Some(fixed_password_slot_kdf_metadata()),
        Some(URL_SAFE_NO_PAD.encode(salt)),
    )
}

fn offline_slot(master_key: &[u8; 32], recovery_key: &[u8; 32], key_id: &str) -> MasterKeySlot {
    wrap_slot(
        master_key,
        &offline_wrapping_key(recovery_key),
        key_id,
        "offline_recovery",
        "hmac-sha256-v1",
        None,
        None,
    )
}

fn unwrap_password(envelope: &MasterKeyEnvelope, password: &str) -> Result<[u8; 32]> {
    let slot = envelope
        .slots
        .iter()
        .find(|slot| slot.slot_type == "password")
        .ok_or_else(|| anyhow!("Vault envelope has no password slot"))?;
    if slot.kdf != "argon2id-v1" {
        bail!("Unsupported password slot KDF");
    }
    if slot.kdf_params.as_ref() != Some(&fixed_password_slot_kdf_metadata()) {
        bail!("Unsupported password slot KDF parameters");
    }
    let salt = URL_SAFE_NO_PAD
        .decode(
            slot.salt
                .as_deref()
                .ok_or_else(|| anyhow!("Password slot has no salt"))?,
        )
        .context("Password slot salt is not valid base64url")?;
    if salt.len() != 16 {
        bail!("Password slot salt has an invalid length");
    }
    unwrap_slot(envelope, slot, &derive_password_slot_key(password, &salt))
}

fn parse_recovery_key(value: &str) -> Result<[u8; 32]> {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = URL_SAFE_NO_PAD
        .decode(compact)
        .context("Vault recovery key is not valid base64url")?;
    decoded.try_into().map_err(|v: Vec<u8>| {
        anyhow!(
            "Vault recovery key must contain 256 bits (received {} bytes)",
            v.len()
        )
    })
}

fn unwrap_offline_recovery(envelope: &MasterKeyEnvelope, recovery_key: &str) -> Result<[u8; 32]> {
    let slot = envelope
        .slots
        .iter()
        .find(|slot| slot.slot_type == "offline_recovery")
        .ok_or_else(|| anyhow!("Vault envelope has no offline recovery slot"))?;
    if slot.kdf != "hmac-sha256-v1" {
        bail!("Unsupported offline recovery slot KDF");
    }
    if slot.kdf_params.is_some() || slot.salt.is_some() {
        bail!("Offline recovery slot has unexpected KDF parameters");
    }
    let recovery_key = parse_recovery_key(recovery_key)?;
    unwrap_slot(envelope, slot, &offline_wrapping_key(&recovery_key))
}

fn validate_envelope(envelope: &MasterKeyEnvelope) -> Result<()> {
    if envelope.version != ENVELOPE_VERSION {
        bail!("Unsupported vault envelope version {}", envelope.version);
    }
    if envelope.revision == 0
        || envelope.revision > JSON_SAFE_INTEGER_MAX
        || !(2..=8).contains(&envelope.slots.len())
    {
        bail!("Vault envelope has invalid revision or slot count");
    }
    if !is_api_identifier(&envelope.key_id, 16) {
        bail!("Vault envelope key ID is invalid");
    }
    let mut ids = HashSet::new();
    let mut password_slots = 0;
    let mut offline_slots = 0;
    for slot in &envelope.slots {
        if !is_api_identifier(&slot.id, 8) || !ids.insert(slot.id.as_str()) {
            bail!("Vault envelope contains an invalid or duplicate slot ID");
        }
        if !(32..=16_384).contains(&slot.wrapped_key.len()) {
            bail!("Vault envelope contains an invalid wrapped key");
        }
        match slot.slot_type.as_str() {
            "password" => password_slots += 1,
            "offline_recovery" => offline_slots += 1,
            "trusted_device" | "webauthn_prf" => {}
            _ => bail!("Vault envelope contains an unsupported slot type"),
        }
    }
    if password_slots != 1 || offline_slots != 1 {
        bail!("Vault envelope must contain one password and one offline recovery slot");
    }
    Ok(())
}

fn is_api_identifier(value: &str, minimum: usize) -> bool {
    (minimum..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn new_envelope(master_key: &[u8; 32], password: &str) -> (MasterKeyEnvelope, String) {
    let mut recovery_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut recovery_key);
    let key_id = master_key_id(master_key);
    let envelope = MasterKeyEnvelope {
        version: ENVELOPE_VERSION,
        revision: 1,
        key_id: key_id.clone(),
        slots: vec![
            password_slot(master_key, password, &key_id),
            offline_slot(master_key, &recovery_key, &key_id),
        ],
        extra: BTreeMap::new(),
    };
    (envelope, URL_SAFE_NO_PAD.encode(recovery_key))
}

fn rotate_password_slot(
    envelope: &MasterKeyEnvelope,
    master_key: &[u8; 32],
    new_password: &str,
) -> Result<MasterKeyEnvelope> {
    validate_envelope(envelope)?;
    if master_key_id(master_key) != envelope.key_id {
        bail!("Trusted device key does not match this vault envelope");
    }
    let mut rotated = envelope.clone();
    rotated.revision = envelope
        .revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("Vault envelope revision overflow"))?;
    rotated.slots.retain(|slot| slot.slot_type != "password");
    rotated
        .slots
        .insert(0, password_slot(master_key, new_password, &envelope.key_id));
    validate_envelope(&rotated)?;
    Ok(rotated)
}

fn envelope_json(envelope: &MasterKeyEnvelope) -> Result<String> {
    validate_envelope(envelope)?;
    serde_json::to_string(envelope).context("Failed to serialize the vault envelope")
}

fn parse_envelope(value: &str) -> Result<MasterKeyEnvelope> {
    let envelope: MasterKeyEnvelope =
        serde_json::from_str(value).context("The vault envelope is not valid JSON")?;
    validate_envelope(&envelope)?;
    Ok(envelope)
}

// Pending two-phase setup/unlock state. Plaintext passwords are never stored.
#[derive(Clone)]
enum PendingVault {
    Setup {
        email: String,
        token: String,
        remember_me: bool,
        master_key: [u8; 32],
        envelope: MasterKeyEnvelope,
        recovery_key: String,
    },
    Locked {
        email: String,
        token: String,
        remember_me: bool,
        envelope: Option<MasterKeyEnvelope>,
        legacy_encrypted_master_key: Option<String>,
    },
}

fn pending_vault() -> &'static Mutex<Option<PendingVault>> {
    static PENDING: OnceLock<Mutex<Option<PendingVault>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(None))
}

fn set_pending(value: Option<PendingVault>) -> Result<()> {
    *pending_vault()
        .lock()
        .map_err(|error| anyhow!("Vault state lock failed: {error}"))? = value;
    Ok(())
}

fn get_pending() -> Result<PendingVault> {
    pending_vault()
        .lock()
        .map_err(|error| anyhow!("Vault state lock failed: {error}"))?
        .clone()
        .ok_or_else(|| anyhow!("No vault setup or unlock is pending"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub email: Option<String>,
    pub master_key_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResult {
    pub success: bool,
    pub email: String,
    pub vault_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_recovery_key: Option<String>,
    pub has_offline_recovery: bool,
    pub legacy_vault: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogoutResult {
    pub cancelled_backups: usize,
}

impl LoginResult {
    fn ready(email: String) -> Self {
        Self {
            success: true,
            email,
            vault_state: "ready".into(),
            vault_recovery_key: None,
            has_offline_recovery: true,
            legacy_vault: false,
            message: None,
        }
    }
}

fn install_authenticated_state(
    state: &AppStateWrapper,
    api: SaveStateClient,
    email: &str,
    master_key: [u8; 32],
    remember_me: bool,
) -> Result<()> {
    let token = api
        .token
        .as_deref()
        .ok_or_else(|| anyhow!("The account session has no token"))?;
    let mut guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
    if crate::backup_operations::session_change_blocked() {
        bail!("An active backup is still bound to the current account. Sign out and stop it before signing in to another account");
    }
    if remember_me {
        save_session(email, token, &master_key)?;
    } else {
        clear_persisted_session();
    }
    guard.api = api;
    guard.email = Some(email.to_string());
    guard.master_key = Some(master_key);
    guard.session_generation = guard.session_generation.wrapping_add(1);
    drop(guard);
    set_pending(None)?;
    crate::kopia::clear_session_cache();
    Ok(())
}

fn install_pending_account_state(
    state: &AppStateWrapper,
    api: SaveStateClient,
    email: &str,
) -> Result<()> {
    let mut guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
    if crate::backup_operations::session_change_blocked() {
        bail!("An active backup is still bound to the current account. Sign out and stop it before signing in to another account");
    }
    guard.api = api;
    guard.email = Some(email.to_string());
    guard.master_key = None;
    guard.session_generation = guard.session_generation.wrapping_add(1);
    crate::kopia::clear_session_cache();
    Ok(())
}

async fn rotate_on_server(
    api: &SaveStateClient,
    envelope: &MasterKeyEnvelope,
    master_key: &[u8; 32],
    new_password: &str,
) -> Result<MasterKeyEnvelope> {
    let rotated = rotate_password_slot(envelope, master_key, new_password)?;
    api.rotate_master_key_envelope(
        envelope.revision,
        &envelope_json(&rotated)?,
        &master_key_verifier(master_key),
    )
    .await?;
    Ok(rotated)
}

async fn reauthenticate_account(
    state: &AppStateWrapper,
    expected_email: &str,
    current_password: &str,
) -> Result<SaveStateClient> {
    let mut api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
        guard.api.clone()
    };
    let response = api.login(expected_email, current_password).await?;
    let actual_email = response.email.as_deref().unwrap_or(expected_email).trim();
    if !actual_email.eq_ignore_ascii_case(expected_email) {
        bail!("Account identity changed during vault unlock");
    }
    Ok(api)
}

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
    let mut api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
        guard.api.clone()
    };
    let trusted = load_session();
    let login = api.login(email, password).await?;
    let account_email = login
        .email
        .as_deref()
        .unwrap_or(email)
        .trim()
        .to_ascii_lowercase();

    if let Some(encoded_envelope) = login.encrypted_master_key_envelope.as_deref() {
        let envelope = parse_envelope(encoded_envelope)?;
        if let Ok(master_key) = unwrap_password(&envelope, password) {
            if login.master_key_requires_original_password {
                rotate_on_server(&api, &envelope, &master_key, password).await?;
            }
            install_authenticated_state(state, api, &account_email, master_key, remember_me)?;
            return Ok(LoginResult::ready(account_email));
        }

        // Credential Manager material is accepted only if its derived keyId
        // matches the authenticated envelope. An old bearer token is no proof.
        if let Some((stored, master_key)) = trusted {
            if stored.email.trim().eq_ignore_ascii_case(&account_email)
                && master_key_id(&master_key) == envelope.key_id
            {
                rotate_on_server(&api, &envelope, &master_key, password).await?;
                install_authenticated_state(state, api, &account_email, master_key, remember_me)?;
                return Ok(LoginResult::ready(account_email));
            }
        }

        let token = login.token.clone();
        install_pending_account_state(state, api, &account_email)?;
        set_pending(Some(PendingVault::Locked {
            email: account_email.clone(),
            token,
            remember_me,
            envelope: Some(envelope),
            legacy_encrypted_master_key: login.encrypted_master_key,
        }))?;
        return Ok(LoginResult {
            success: true,
            email: account_email,
            vault_state: "locked".into(),
            vault_recovery_key: None,
            has_offline_recovery: true,
            legacy_vault: false,
            message: Some("Account recovered, vault locked. Use the previous vault password or your offline vault recovery key. Account recovery codes cannot decrypt backups.".into()),
        });
    }

    if let Some(legacy) = login.encrypted_master_key.as_deref() {
        if let Ok(master_key) = decrypt_master_key(legacy, password) {
            let (envelope, recovery_key) = new_envelope(&master_key, password);
            let token = login.token.clone();
            install_pending_account_state(state, api, &account_email)?;
            set_pending(Some(PendingVault::Setup {
                email: account_email.clone(),
                token,
                remember_me,
                master_key,
                envelope,
                recovery_key: recovery_key.clone(),
            }))?;
            return Ok(LoginResult {
                success: true,
                email: account_email,
                vault_state: "recovery_key_ack_required".into(),
                vault_recovery_key: Some(recovery_key),
                has_offline_recovery: true,
                legacy_vault: true,
                message: Some("Save this 256-bit vault recovery key. It is shown once and is separate from account recovery codes.".into()),
            });
        }

        let token = login.token.clone();
        install_pending_account_state(state, api, &account_email)?;
        set_pending(Some(PendingVault::Locked {
            email: account_email.clone(),
            token,
            remember_me,
            envelope: None,
            legacy_encrypted_master_key: Some(legacy.to_string()),
        }))?;
        return Ok(LoginResult {
            success: true,
            email: account_email,
            vault_state: "locked".into(),
            vault_recovery_key: None,
            has_offline_recovery: false,
            legacy_vault: true,
            message: Some("Account recovered, legacy vault locked. This vault predates offline recovery keys, so only its previous vault password can decrypt it. Resetting account access did not change or erase its ciphertext.".into()),
        });
    }

    let mut master_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut master_key);
    // Keep legacy ciphertext initialize-once for older clients. Recovery never
    // replaces it, so an account-auth reset cannot destroy encrypted backups.
    api.save_master_key(&encrypt_master_key(&master_key, password))
        .await?;
    let (envelope, recovery_key) = new_envelope(&master_key, password);
    let token = login.token.clone();
    install_pending_account_state(state, api, &account_email)?;
    set_pending(Some(PendingVault::Setup {
        email: account_email.clone(),
        token,
        remember_me,
        master_key,
        envelope,
        recovery_key: recovery_key.clone(),
    }))?;
    Ok(LoginResult {
        success: true,
        email: account_email,
        vault_state: "recovery_key_ack_required".into(),
        vault_recovery_key: Some(recovery_key),
        has_offline_recovery: true,
        legacy_vault: false,
        message: Some("Save this 256-bit vault recovery key. It is shown once and is separate from account recovery codes.".into()),
    })
}

#[tauri::command]
pub async fn cmd_confirm_vault_recovery_key(
    state: tauri::State<'_, AppStateWrapper>,
    account_password: String,
    acknowledged: bool,
) -> std::result::Result<LoginResult, String> {
    confirm_vault_recovery_key_inner(&state, &account_password, acknowledged)
        .await
        .map_err(|e| e.to_string())
}

async fn confirm_vault_recovery_key_inner(
    state: &AppStateWrapper,
    account_password: &str,
    acknowledged: bool,
) -> Result<LoginResult> {
    if !acknowledged {
        bail!("Confirm that the vault recovery key has been saved before continuing");
    }
    let PendingVault::Setup {
        email,
        token,
        remember_me,
        master_key,
        envelope,
        recovery_key,
    } = get_pending()?
    else {
        bail!("No vault recovery key acknowledgement is pending");
    };
    let recovered = unwrap_offline_recovery(&envelope, &recovery_key)?;
    if recovered != master_key {
        bail!("Pending vault recovery key does not match the account key");
    }
    let api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
        guard.api.clone()
    };
    if api.token.as_deref() != Some(token.as_str()) {
        bail!("The account session changed before vault setup completed");
    }
    api.initialize_master_key_envelope(
        account_password,
        &envelope_json(&envelope)?,
        &master_key_verifier(&master_key),
    )
    .await?;
    install_authenticated_state(state, api, &email, master_key, remember_me)?;
    Ok(LoginResult::ready(email))
}

#[tauri::command]
pub async fn cmd_unlock_vault(
    state: tauri::State<'_, AppStateWrapper>,
    method: String,
    secret: String,
    account_password: String,
) -> std::result::Result<LoginResult, String> {
    unlock_vault_inner(&state, &method, &secret, &account_password)
        .await
        .map_err(|e| e.to_string())
}

async fn unlock_vault_inner(
    state: &AppStateWrapper,
    method: &str,
    secret: &str,
    account_password: &str,
) -> Result<LoginResult> {
    let PendingVault::Locked {
        email,
        token,
        remember_me,
        envelope,
        legacy_encrypted_master_key,
    } = get_pending()?
    else {
        bail!("No locked vault is pending");
    };
    let current_api = {
        let guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
        guard.api.clone()
    };
    if current_api.token.as_deref() != Some(token.as_str()) {
        bail!("The account session changed before the vault was unlocked");
    }
    // The proof endpoint authenticates AMK possession. Reauthenticate as well
    // so a typo cannot create a password slot that differs from account login.
    let api = reauthenticate_account(state, &email, account_password).await?;

    if let Some(envelope) = envelope {
        let master_key = match method {
            "vault_password" => unwrap_password(&envelope, secret)?,
            "vault_recovery_key" => unwrap_offline_recovery(&envelope, secret)?,
            _ => bail!("Choose the previous vault password or offline vault recovery key"),
        };
        rotate_on_server(&api, &envelope, &master_key, account_password).await?;
        install_authenticated_state(state, api, &email, master_key, remember_me)?;
        return Ok(LoginResult::ready(email));
    }

    if method != "vault_password" {
        bail!("This legacy vault has no offline recovery key; use its previous vault password");
    }
    let legacy = legacy_encrypted_master_key
        .as_deref()
        .ok_or_else(|| anyhow!("The legacy vault ciphertext is missing"))?;
    let master_key = decrypt_master_key(legacy, secret)?;
    // A pre-envelope account has no server-verifiable AMK commitment. Unlock
    // locally, but do not let a reset bearer session create a replacement AMK.
    install_authenticated_state(state, api, &email, master_key, remember_me)?;
    Ok(LoginResult {
        success: true,
        email,
        vault_state: "ready_legacy".into(),
        vault_recovery_key: None,
        has_offline_recovery: false,
        legacy_vault: true,
        message: Some("Legacy vault unlocked with its previous password. Account recovery did not change its encryption. This pre-envelope vault still requires that previous vault password on a new device.".into()),
    })
}

/// Abandon pending login without deleting the retained Credential Manager AMK.
fn invalidate_in_memory_session_preserving_trusted_device(state: &AppStateWrapper) -> Result<()> {
    {
        let mut guard = state.0.lock().map_err(|e| anyhow!("Lock error: {e}"))?;
        guard.api.clear_token();
        guard.email = None;
        guard.master_key = None;
        guard.session_generation = guard.session_generation.wrapping_add(1);
    }
    set_pending(None)?;
    crate::kopia::clear_session_cache();
    Ok(())
}

#[tauri::command]
pub async fn cmd_abandon_vault_unlock(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<(), String> {
    invalidate_in_memory_session_preserving_trusted_device(&state).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cmd_logout(
    state: tauri::State<'_, AppStateWrapper>,
    logout_token: String,
) -> std::result::Result<LogoutResult, String> {
    let backup_guard = crate::backup_operations::stop_for_logout(&logout_token)
        .await
        .map_err(|error| error.to_string())?;
    {
        let mut guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
        guard.api.clear_token();
        guard.email = None;
        guard.master_key = None;
        guard.session_generation = guard.session_generation.wrapping_add(1);
    }
    set_pending(None).map_err(|e| e.to_string())?;
    clear_persisted_session();
    crate::kopia::clear_session_cache();
    Ok(LogoutResult {
        cancelled_backups: backup_guard.cancelled_count,
    })
}

#[tauri::command]
pub fn cmd_prepare_logout() -> std::result::Result<crate::backup_operations::PreparedLogout, String>
{
    crate::backup_operations::prepare_logout().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn cmd_abort_logout(logout_token: String) -> std::result::Result<(), String> {
    crate::backup_operations::abort_logout(&logout_token).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_get_auth_status(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<AuthStatus, String> {
    let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
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
        let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
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
        let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
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
        let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
        guard.api.clone()
    };
    let resp = api.resume_subscription().await.map_err(|e| e.to_string())?;
    Ok(resp)
}

/// An auth_version 401 must lead to account reauthentication, not cmd_logout,
/// so the remembered AMK can still prove and unlock the client-owned vault.
pub fn try_restore_session(state: &AppStateWrapper) {
    let _ = std::fs::remove_file(legacy_creds_path());
    if let Some((session, master_key)) = load_session() {
        if let Ok(mut guard) = state.0.lock() {
            guard.api.set_token(session.token);
            guard.email = Some(session.email.trim().to_ascii_lowercase());
            guard.master_key = Some(master_key);
            guard.session_generation = guard.session_generation.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_master_key_wrapper_remains_compatible() {
        let master_key = [0x5Au8; 32];
        let encrypted = encrypt_master_key(&master_key, "correct horse battery staple");
        assert_eq!(
            decrypt_master_key(&encrypted, "correct horse battery staple").unwrap(),
            master_key
        );
        assert!(decrypt_master_key(&encrypted, "wrong password").is_err());
    }

    #[test]
    fn password_and_256_bit_offline_slots_unwrap_the_same_amk() {
        let master_key = [0xA5u8; 32];
        let password = "this is a strong vault password";
        let (envelope, recovery_key) = new_envelope(&master_key, password);
        assert_eq!(unwrap_password(&envelope, password).unwrap(), master_key);
        assert_eq!(
            unwrap_offline_recovery(&envelope, &recovery_key).unwrap(),
            master_key
        );
        assert_eq!(parse_recovery_key(&recovery_key).unwrap().len(), 32);
        assert_eq!(master_key_verifier(&master_key).len(), 43);
    }

    #[test]
    fn password_slot_argon2id_profile_has_a_stable_known_vector() {
        let derived = derive_password_slot_key("password", b"somesalt12345678");
        assert_eq!(
            hex::encode(derived),
            "0c4c0b6db219194b6006e078818a24eabea136f7af619a31930310e7f2d749a5"
        );
    }

    #[test]
    fn amk_verifier_domain_has_a_stable_api_compatible_vector() {
        assert_eq!(
            master_key_verifier(&[0u8; 32]),
            "dU5XOUkmjT0f0ZEjNP6pMpUMLcoGZa9dYrBSmxiE66o"
        );
    }

    #[test]
    fn envelope_rotation_preserves_offline_slot_and_changes_password_slot() {
        let master_key = [7u8; 32];
        let (envelope, recovery_key) = new_envelope(&master_key, "old password");
        let old_offline = envelope
            .slots
            .iter()
            .find(|slot| slot.slot_type == "offline_recovery")
            .unwrap()
            .clone();
        let rotated = rotate_password_slot(&envelope, &master_key, "new password").unwrap();
        assert_eq!(rotated.revision, envelope.revision + 1);
        assert_eq!(
            rotated
                .slots
                .iter()
                .find(|slot| slot.slot_type == "offline_recovery")
                .unwrap(),
            &old_offline
        );
        assert!(unwrap_password(&rotated, "old password").is_err());
        assert_eq!(
            unwrap_password(&rotated, "new password").unwrap(),
            master_key
        );
        assert_eq!(
            unwrap_offline_recovery(&rotated, &recovery_key).unwrap(),
            master_key
        );
    }

    #[test]
    fn slot_ciphertext_and_metadata_tampering_is_rejected() {
        let master_key = [9u8; 32];
        let (envelope, recovery_key) = new_envelope(&master_key, "password");
        let mut ciphertext_tampered = envelope.clone();
        let replacement = if ciphertext_tampered.slots[0].wrapped_key.starts_with('A') {
            "B"
        } else {
            "A"
        };
        ciphertext_tampered.slots[0]
            .wrapped_key
            .replace_range(0..1, replacement);
        assert!(unwrap_password(&ciphertext_tampered, "password").is_err());

        let mut id_tampered = envelope.clone();
        id_tampered.slots[1].id = random_id();
        assert!(unwrap_offline_recovery(&id_tampered, &recovery_key).is_err());

        let mut key_id_tampered = envelope.clone();
        key_id_tampered.key_id = random_id();
        assert!(unwrap_password(&key_id_tampered, "password").is_err());

        let wrong_recovery_key = URL_SAFE_NO_PAD.encode([0xEEu8; 32]);
        assert!(unwrap_offline_recovery(&envelope, &wrong_recovery_key).is_err());
    }

    #[test]
    fn malformed_versions_duplicate_slots_and_trusted_key_mismatch_are_rejected() {
        let master_key = [3u8; 32];
        let (mut envelope, _) = new_envelope(&master_key, "password");
        envelope.version = 2;
        assert!(validate_envelope(&envelope).is_err());

        let (mut envelope, _) = new_envelope(&master_key, "password");
        envelope.slots[1].id = envelope.slots[0].id.clone();
        assert!(validate_envelope(&envelope).is_err());

        let (envelope, _) = new_envelope(&master_key, "password");
        assert!(rotate_password_slot(&envelope, &[4u8; 32], "new").is_err());
    }

    #[test]
    fn serialized_envelope_matches_api_shape_and_round_trips() {
        let (envelope, _) = new_envelope(&[11u8; 32], "password");
        let json = envelope_json(&envelope).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["revision"], 1);
        assert!(value["keyId"].as_str().unwrap().len() >= 16);
        assert_eq!(value["slots"][0]["type"], "password");
        assert_eq!(value["slots"][1]["type"], "offline_recovery");
        assert_eq!(parse_envelope(&json).unwrap(), envelope);
    }

    #[test]
    fn auth_version_invalidation_makes_scheduler_unauthenticated_without_logout() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let mut app_state = crate::state::AppState::new(SaveStateClient::new("test".into()), db);
        app_state.api.set_token("now-invalid-token".into());
        app_state.email = Some("owner@example.com".into());
        app_state.master_key = Some([31u8; 32]);
        let wrapper = AppStateWrapper(Mutex::new(app_state));

        invalidate_in_memory_session_preserving_trusted_device(&wrapper).unwrap();
        let guard = wrapper.0.lock().unwrap();
        assert!(guard.api.token.is_none());
        assert!(guard.email.is_none());
        assert!(guard.master_key.is_none());
        assert!(guard.account_scope().is_none());
        // The helper deliberately has no Credential Manager deletion path;
        // only explicit cmd_logout calls clear_persisted_session().
    }

    #[test]
    fn concurrent_password_rotations_keep_the_same_optimistic_base_revision() {
        let master_key = [21u8; 32];
        let (envelope, _) = new_envelope(&master_key, "old");
        let first = rotate_password_slot(&envelope, &master_key, "first").unwrap();
        let concurrent = rotate_password_slot(&envelope, &master_key, "second").unwrap();

        // Both client requests must send expectedRevision=1 with revision=2.
        // The API's conditional update accepts one and returns 409 for the
        // other; the client never silently rebases stale ciphertext to 3.
        assert_eq!(envelope.revision, 1);
        assert_eq!(first.revision, 2);
        assert_eq!(concurrent.revision, 2);
        assert_ne!(first.slots[0].wrapped_key, concurrent.slots[0].wrapped_key);
    }
}
