const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const native = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');

test('Databases is a first-class app page with a connection-first setup flow', () => {
  assert.match(html, /data-view="databases"/);
  assert.match(html, /id="page-databases"/);
  assert.match(html, /id="btn-test-database"/);
  assert.match(html, /id="database-selection-section"[^>]*hidden/);
  assert.match(html, /id="database-schedule-section"[^>]*hidden/);
  assert.match(html, /id="btn-save-database"[^>]*disabled/);
  assert.doesNotMatch(html, /id="database-(?:setup|form)"[^>]*modal/);
});

test('database selection exposes all requested scopes and fidelity controls', () => {
  assert.match(html, /value="all"[^>]*checked/);
  assert.match(html, /value="databases"/);
  assert.match(html, /value="tables"/);
  assert.match(html, /id="database-include-new"[^>]*checked/);
  assert.match(html, /id="database-include-create"[^>]*checked/);
  assert.match(html, /id="database-include-users"/);
  assert.match(app, /cmd_list_database_tables/);
  assert.match(app, /cmd_test_database_connection/);
});

test('passwords are separate from connection URLs and saves require a fresh test', () => {
  assert.match(html, /mysql:\/\/root@127\.0\.0\.1:3306/);
  assert.match(html, /type="password"[^>]*id="database-password"/);
  assert.match(html, /Do not put the password in this field/);
  assert.match(app, /databaseConnectionFingerprint/);
  assert.match(app, /A successful connection test is required/);
});

test('database SQL is piped straight from the dump tool into Kopia', () => {
  assert.match(native, /source_child[\s\S]+stdout[\s\S]+kopia_child[\s\S]+stdin/);
  assert.match(native, /std::io::copy\(&mut source_stdout, &mut kopia_stdin\)/);
  assert.match(native, /--stdin-file=/);
  assert.match(native, /"-"\.to_string\(\)/);
  assert.doesNotMatch(native, /database\.sql[\s\S]{0,120}(?:temp_dir|NamedTempFile|File::create)/);
});

test('database restore is streamed into the database client and can be stopped', () => {
  assert.match(native, /std::io::copy\(&mut kopia_stdout, &mut target_stdin\)/);
  assert.match(app, /cmd_restore_database_backup/);
  assert.match(app, /cmd_cancel_restore/);
});

test('database failures are formatted centrally without breaking tool discovery', () => {
  assert.match(app, /function friendlyError[\s\S]+database_authentication_failed/);
  const discovery = app.match(/async function loadDatabaseTools[\s\S]+?\n}\r?\n\r?\nfunction selectedDatabaseTool/)?.[0] || '';
  assert.ok(discovery);
  assert.doesNotMatch(discovery, /lower\.includes|raw\.split/);
});
