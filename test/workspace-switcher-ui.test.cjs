const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.join(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const styles = fs.readFileSync(path.join(root, 'src', 'styles.css'), 'utf8');
const state = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'state.rs'), 'utf8');

test('lower-left switcher selects vaults and still reaches organization accounts', () => {
  assert.match(html, /id="workspace-trigger"[^>]+aria-haspopup="menu"/);
  assert.match(html, /id="workspace-menu"[^>]+role="menu"/);
  assert.match(app, /data-vault-id="\$\{escapeHtml\(vault\.id\)\}"/);
  assert.match(app, /role="menuitemradio"/);
  assert.match(app, /function selectVault\(vaultId\)/);
  assert.match(app, /data-vault-action="add"/);
  assert.match(app, /cmd_list_account_workspaces/);
  assert.match(app, /cmd_switch_account_workspace/);
  assert.match(app, /workspace\.kind === 'organization'/);
  assert.match(styles, /\.workspace-menu/);
});

test('customer-owned vault hides managed-only pages instead of mixing storage', () => {
  assert.match(html, /data-view="databases" data-vault-scope="managed"/);
  assert.match(html, /data-view="backup" data-vault-scope="managed"/);
  assert.match(html, /data-view="backups" data-vault-scope="managed"/);
  assert.match(html, /id="custom-vault-dashboard"/);
  assert.match(app, /function syncVaultContextUi\(\)/);
  assert.match(app, /function selectVault\(vaultId\)[\s\S]*?getElementById\('byos-snapshots'\)\.replaceChildren\(\)/);
  assert.match(app, /if \(selectedVaultId && selectedVaultId !== vaultModel\.MANAGED_VAULT_ID[\s\S]*?\['databases', 'backup', 'backups'\]\.includes\(pageId\)\) pageId = 'profiles'/);
  assert.match(app, /vaultManagerOpen = false/);
});

test('local profile ownership includes the active service workspace', () => {
  assert.match(state, /format!\("\{email\}::\{workspace_id\}"\)/);
  assert.match(state, /self\.api\.workspace_id\(\)/);
});

test('switching invalidates repository and visible workspace state', () => {
  assert.match(app, /repositorySessionGeneration \+= 1/);
  assert.match(app, /currentFolder = '\/'/);
  assert.match(app, /folderList = \[\]/);
  assert.match(app, /workspaceUiGeneration \+= 1/);
  assert.match(app, /byosVaults = \[\]/);
  assert.match(app, /document\.getElementById\('byos-form'\)\.reset\(\)/);
  assert.match(app, /document\.getElementById\('profile-modal'\)\.classList\.add\('hidden'\)/);
});
