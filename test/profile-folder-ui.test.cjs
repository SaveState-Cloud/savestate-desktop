const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const profiles = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'profiles.rs'), 'utf8');
const databases = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'databases.rs'), 'utf8');
const backup = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'backup.rs'), 'utf8');
const kopia = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');
const styles = fs.readFileSync(path.join(root, 'src', 'styles.css'), 'utf8');

test('profiles use an automatic folder instead of a user-selected destination', () => {
  assert.match(html, /id="profile-folder-preview"/);
  assert.doesNotMatch(html, /id="profile-folder"/);
  assert.match(app, /function profileFolderName/);
  assert.match(profiles, /ensure_profile_folder\(&profile\.id, &profile\.name\)/);
  assert.match(databases, /ensure_profile_folder\(&profile\.id, &profile\.name\)/);
});

test('profile deletion defaults to preserving backups and offers the exact destructive choice', () => {
  assert.match(html, /Delete all backups within this profile/);
  assert.match(html, /Backups moved elsewhere are preserved/);
  assert.match(app, /const deleteBackups = document\.getElementById\('profile-delete-backups'\)\.checked/);
  assert.match(app, /deleteBackups \}/);
  assert.match(profiles, /delete_backups\.unwrap_or\(false\)/);
  assert.match(databases, /delete_backups\.unwrap_or\(false\)/);
});

test('folder deletion is recursive while moved-out snapshots are outside the match', () => {
  assert.match(backup, /folder_contains\(folder, &snapshot\.folder\)/);
  assert.match(backup, /candidate == folder/);
  assert.match(backup, /suffix\.starts_with\('\/'\)/);
  assert.match(app, /Delete folder and contents/);
});

test('profile backups carry stable identity metadata and display version labels newest first', () => {
  assert.match(kopia, /--tags=savestate-profile:/);
  assert.match(kopia, /--tags=savestate-trigger:/);
  assert.match(kopia, /--keep-latest=2147483647/);
  assert.match(app, /Newest · v\$\{version\}/);
  assert.match(app, /Oldest · v\$\{version\}/);
  assert.match(app, /new Date\(right\.lastModified\) - new Date\(left\.lastModified\)/);
});

test('backup filename content does not replace table-cell layout', () => {
  assert.match(app, /nameContent\.className = 'backup-name-content'/);
  assert.match(app, /tdName\.appendChild\(nameContent\)/);
  assert.match(styles, /\.backup-name-cell\s*\{\s*vertical-align: middle;/);
  assert.match(styles, /\.backup-name-content\s*\{\s*display: flex;/);
  assert.doesNotMatch(styles, /\.backup-name-cell\s*\{[^}]*display:\s*flex;/);
});

test('quick backups keep root available and cannot target another managed profile folder', () => {
  assert.match(html, /id="quick-backup-folder"[\s\S]*?value="\/"/);
  assert.match(app, /const isManagedProfileFolder = typeof f !== 'string' && f\.managed/);
  assert.match(app, /managedByOtherProfile/);
});

test('folder and backup actions remain named and keyboard reachable', () => {
  assert.match(app, /openButton\.className = 'folder-card-open'/);
  assert.match(app, /openButton\.setAttribute\('aria-label', `Open folder \$\{sf\.name\}`\)/);
  assert.match(app, /delBtn\.setAttribute\('aria-label', `Delete folder \$\{sf\.name\}`\)/);
  assert.match(app, /moveBtn\.setAttribute\('aria-label', `Move \$\{b\.filename\} to another folder`\)/);
  assert.match(styles, /\.folder-card-open:focus-visible/);
  assert.match(styles, /\.folder-card:focus-within \.folder-card-delete/);
});
