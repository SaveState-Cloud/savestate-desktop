const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const root = path.resolve(__dirname, '..');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const native = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');

function element() {
  const classes = new Set();
  return {
    textContent: '',
    classList: {
      toggle(name, enabled) {
        if (enabled) classes.add(name);
        else classes.delete(name);
      },
      contains(name) { return classes.has(name); },
    },
  };
}

test('cleanup badge shows queued work, explains failures, and clears on completion', () => {
  const ids = new Map([
    ['cleanup-state', element()],
    ['cleanup-state-text', element()],
    ['btn-retry-cleanup', element()],
  ]);
  const source = app.match(/function setStorageCleanupState\(status, message = ''\) \{[\s\S]*?\n\}/)?.[0];
  assert.ok(source, 'cleanup renderer exists');
  const render = vm.runInNewContext(`(${source})`, {
    document: { getElementById: id => ids.get(id) },
  });

  render('pending', 'Waiting for another vault');
  assert.equal(ids.get('cleanup-state').classList.contains('hidden'), false);
  assert.equal(ids.get('cleanup-state-text').textContent, 'Waiting for another vault');
  assert.equal(ids.get('btn-retry-cleanup').classList.contains('hidden'), true);

  render('failed', 'Repository could not be opened');
  assert.equal(ids.get('cleanup-state').classList.contains('failed'), true);
  assert.equal(ids.get('btn-retry-cleanup').classList.contains('hidden'), false);

  render('complete', 'Done');
  assert.equal(ids.get('cleanup-state').classList.contains('hidden'), true);
  assert.equal(ids.get('cleanup-state').classList.contains('failed'), false);
});

test('native cleanup status is scoped and the dashboard can refresh or retry it', () => {
  assert.match(native, /pub fn cmd_get_storage_cleanup_status/);
  assert.match(native, /account_scope: scope\.to_string\(\)/);
  assert.match(app, /cleanup\.accountScope !== storageCleanupScope/);
  assert.match(app, /invoke\('cmd_get_storage_cleanup_status'\)/);
  assert.match(app, /invoke\('cmd_schedule_storage_cleanup', \{ force: true \}\)/);
  assert.match(html, /id="btn-retry-cleanup"/);
});
