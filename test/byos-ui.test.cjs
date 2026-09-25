const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const read = (...parts) => fs.readFileSync(path.join(root, ...parts), 'utf8');
const html = read('src', 'index.html');
const app = read('src', 'app.js');
const native = read('src-tauri', 'src', 'byos.rs');
const profiles = read('src-tauri', 'src', 'profiles.rs');

test('Vaults own customer storage, backup sources, and schedules', () => {
  assert.match(html, /id="vault-overview"/);
  assert.match(html, /id="vault-detail"/);
  assert.match(html, /id="byos-form"/);
  assert.match(html, /id="profiles-list"/);
  assert.ok(html.indexOf('id="byos-form"') < html.indexOf('id="page-settings"'));
  assert.match(html, /id="byos-secret"[^>]*type="password"/);
  assert.match(html, /type="hidden" id="profile-vault"/);
  assert.match(app, /invoke\('cmd_byos_add_vault', \{ input: \{/);
  assert.match(app, /invoke\('cmd_create_profile', \{ name, sourcePath, schedule, retention, folder: '\/', vaultId \}\)/);
  assert.match(app, /vaultModel\.profilesInVault\(profiles, selectedVaultId\)/);
  assert.match(profiles, /if let Some\(vault_id\) = vault_id\.as_deref\(\)/);
  assert.match(profiles, /if byos_profile\.vault_id\.is_some\(\)/);
});

test('customer-owned secrets stay local and restores cannot overwrite an existing folder', () => {
  assert.match(native, /credential_entry\(&vault\.owner_account, &vault\.id\)/);
  assert.match(native, /serde_json::to_vec\(&credentials\)/);
  assert.match(native, /std::fs::create_dir\(&restore_path\)/);
  assert.match(native, /The bucket or local folder and its backups remain the customer's property/);
  assert.match(app, /custom vaults use storage you control; their bytes do not count toward your managed storage allowance/i);
});

test('an expired subscription still reaches Vaults and can only reconnect existing storage', () => {
  assert.match(app, /serviceWorkspaceReady = Boolean\(result\.service_workspace_ready\)/);
  assert.match(app, /if \(serviceWorkspaceReady\)/);
  assert.match(app, /navigateTo\('profiles'\)/);
  assert.match(app, /Reconnect Vault/);
  assert.match(native, /connect\([\s\S]*?&context\.repository_password,[\s\S]*?active && !had_marker,[\s\S]*?local && !had_marker,[\s\S]*?\)/);
  assert.match(native, /AccountContext::capture_byos/);
  assert.match(native, /if mode == "backup" \{\s*verify_entitlement/);
});

test('external drives are real filesystem vaults and never require cloud keys', () => {
  assert.match(html, /<option value="filesystem">External drive<\/option>/);
  assert.match(html, /id="btn-byos-choose-folder"/);
  assert.match(app, /invoke\('cmd_byos_relocate_vault'/);
  assert.match(native, /"filesystem"\.into\(\), format!\("--path=\{path\}"\)/);
  assert.match(native, /EXTERNAL_DRIVE_CHANGED/);
  assert.match(native, /ensure_source_outside_vault\(source, &session\)/);
  assert.match(app, /selectedVault\.available === false/);
  assert.match(app, /vault\.available === false \? 'Drive disconnected'/);
});

test('vault creation is not numerically capped and plan-check errors are distinct', () => {
  assert.doesNotMatch(native, /destination safety limit|max_vaults/);
  assert.match(native, /Plan check is unavailable/);
  assert.match(app, /Checking your plan\. Your existing vaults and restore points are available now/);
  assert.match(app, /Promise\.race\(\[/);
  assert.ok(app.indexOf('renderVaultOverview([], [], null, vaultError, true, true)') < app.indexOf('await localProfilesPromise'));
  assert.match(app, /if \(signature === lastVaultRowsSignature\)/);
});

test('backup-source dialog moves and contains keyboard focus', () => {
  assert.match(html, /class="modal glass-card" role="dialog" aria-modal="true" aria-labelledby="profile-modal-title"/);
  assert.match(app, /profileModalReturnFocus = document\.activeElement/);
  assert.match(app, /document\.getElementById\('profile-name'\)\.focus\(\)/);
  assert.match(app, /document\.getElementById\('profile-modal'\)\.addEventListener\('keydown'/);
  assert.match(app, /if \(event\.key === 'Escape'\)/);
});

test('the visible profile page refreshes after background scheduled backups', () => {
  assert.match(app, /PROFILE_REFRESH_INTERVAL_MS = 30 \* 1000/);
  assert.match(app, /setInterval\(refreshVisibleProfiles, PROFILE_REFRESH_INTERVAL_MS\)/);
  assert.match(app, /window\.addEventListener\('focus',[\s\S]*?refreshVisibleProfiles\(\)/);
  assert.match(app, /function refreshVisibleProfiles\(\)[\s\S]*?pages\.profiles\?\.classList\.contains\('active'\)[\s\S]*?void loadProfiles\(\)/);
});
