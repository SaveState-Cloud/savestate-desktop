const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.join(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const styles = fs.readFileSync(path.join(root, 'src', 'styles.css'), 'utf8');
const state = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'state.rs'), 'utf8');

test('sidebar exposes an accessible personal and organization workspace switcher', () => {
  assert.match(html, /id="workspace-trigger"[^>]+aria-haspopup="listbox"/);
  assert.match(html, /id="workspace-menu"[^>]+role="listbox"/);
  assert.match(app, /cmd_list_account_workspaces/);
  assert.match(app, /cmd_switch_account_workspace/);
  assert.match(app, /workspace\.kind === 'organization'/);
  assert.match(styles, /\.workspace-menu/);
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
});
