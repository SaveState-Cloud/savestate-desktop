const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const kopia = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');
const profiles = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'profiles.rs'), 'utf8');
const databases = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'databases.rs'), 'utf8');

test('profile command preserves nested API error codes for the UI', () => {
  assert.match(profiles, /result\.map_err\(\|error\| format!\("\{error:#\}"\)\)/);
  assert.match(databases, /result\.map_err\(\|error\| format!\("\{error:#\}"\)\)/);
});

test('backup terminal event carries the actionable error and disarms the generic guard', () => {
  const failureArm = kopia.match(/Err\(error\) => \{[\s\S]*?engine_job\.fail_with_error\("backup_failed"[\s\S]*?Err\(error\)\r?\n\s*\}/)?.[0] || '';
  assert.match(failureArm, /emit_progress\([\s\S]*?"error"[\s\S]*?format!\("\{error:#\}"\)/);
  assert.match(failureArm, /terminal_progress\.finish\(\)/);
});

test('database backups use the same detailed terminal failure contract', () => {
  const failureArm = kopia.match(/Err\(error\) => \{\r?\n\s*emit_progress\(app, &op_id, "error", 0\.0, &format!\("\{error:#\}"\)\);[\s\S]*?engine_job\.fail_with_error\("database_backup_failed"[\s\S]*?Err\(error\)\r?\n\s*\}/)?.[0] || '';
  assert.match(failureArm, /terminal_progress\.finish\(\)/);
});

test('source allowance rejection is translated into an actionable workspace message', () => {
  assert.match(app, /source_quota_exceeded/);
  assert.match(app, /select a smaller source/);
  assert.match(app, /ask the workspace owner for more storage/);
});
