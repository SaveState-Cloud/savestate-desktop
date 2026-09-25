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

test('Settings connects customer-owned storage and profile creation chooses its destination', () => {
  assert.match(html, /id="byos-section"/);
  assert.match(html, /id="byos-secret"[^>]*type="password"/);
  assert.match(html, /id="profile-vault"/);
  assert.match(app, /invoke\('cmd_byos_add_vault', \{ input: \{/);
  assert.match(app, /invoke\('cmd_create_profile', \{ name, sourcePath, schedule, retention, folder: '\/', vaultId \}\)/);
  assert.match(profiles, /if let Some\(vault_id\) = vault_id\.as_deref\(\)/);
  assert.match(profiles, /if byos_profile\.vault_id\.is_some\(\)/);
});

test('customer-owned secrets stay local and restores cannot overwrite an existing folder', () => {
  assert.match(native, /credential_entry\(&vault\.owner_account, &vault\.id\)/);
  assert.match(native, /serde_json::to_vec\(&credentials\)/);
  assert.match(native, /std::fs::create_dir\(&restore_path\)/);
  assert.match(native, /The remote bucket and its backups remain the customer's property/);
  assert.match(app, /Customer-owned bytes do not count toward your SaveState-managed storage allowance/);
});

test('an expired subscription still reaches Settings and can only reconnect existing storage', () => {
  assert.match(app, /serviceWorkspaceReady = Boolean\(result\.service_workspace_ready\)/);
  assert.match(app, /if \(serviceWorkspaceReady\)/);
  assert.match(app, /navigateTo\('settings'\)/);
  assert.match(app, /Reconnect existing storage/);
  assert.match(native, /connect\(&app, &session, &context\.repository_password, active, None\)/);
  assert.match(native, /AccountContext::capture_byos/);
  assert.match(native, /if mode == "backup" \{\s*verify_entitlement/);
});

test('the visible profile page refreshes after background scheduled backups', () => {
  assert.match(app, /PROFILE_REFRESH_INTERVAL_MS = 30 \* 1000/);
  assert.match(app, /setInterval\(refreshVisibleProfiles, PROFILE_REFRESH_INTERVAL_MS\)/);
  assert.match(app, /window\.addEventListener\('focus',[\s\S]*?refreshVisibleProfiles\(\)/);
  assert.match(app, /function refreshVisibleProfiles\(\)[\s\S]*?pages\.profiles\?\.classList\.contains\('active'\)[\s\S]*?void loadProfiles\(\)/);
});
