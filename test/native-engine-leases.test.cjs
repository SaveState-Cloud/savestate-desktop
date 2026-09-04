const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..', 'src-tauri', 'src');
const read = file => fs.readFileSync(path.join(root, file), 'utf8');
const kopia = read('kopia.rs');

// Companion wiring checks for the executable Rust queued-writer regression.
// Async native HTTP workflows require the installed app; these assertions make
// accidental gate re-entry at their internal call boundary visible in CI.
function body(source, name) {
  const start = source.indexOf('fn ' + name + '(');
  assert.notEqual(start, -1, name);
  const end = source.indexOf('\n}', start);
  return source.slice(start, end + 2);
}

test('already admitted engine APIs require a lease and never reacquire it', () => {
  for (const name of [
    'backup_paths_with_operation', 'backup_stream_with_operation',
    'delete_snapshot_with_lease', 'delete_snapshot_with_context',
    'prune_profile_snapshots_with_operation', 'restore_snapshot_with_lease',
    'restore_database_snapshot_to_command', 'set_retention_with_operation',
    'set_retention_with_context',
  ]) {
    const code = body(kopia, name);
    assert.match(code, /&EngineLease</, name);
    assert.doesNotMatch(code, /begin_operation\(/, name);
  }
  const deletion = body(read('backup.rs'), 'delete_snapshots_in_folder');
  assert.match(deletion, /&crate::kopia::EngineLease</);
  assert.doesNotMatch(deletion, /begin_operation\(/);
});

test('outer workflows propagate their lease into every nested engine step', () => {
  const cases = [
    ['profiles.rs', 'run_profile_backup_with_context', ['backup_paths_with_operation', 'prune_profile_snapshots_with_operation']],
    ['databases.rs', 'run_database_backup_with_context', ['backup_stream_with_operation', 'prune_profile_snapshots_with_operation']],
    ['databases.rs', 'cmd_restore_database_backup', ['restore_database_snapshot_to_command']],
    ['restore.rs', 'cmd_restore_backup', ['restore_snapshot_with_lease']],
    ['profiles.rs', 'cmd_delete_profile', ['delete_snapshots_in_folder']],
    ['databases.rs', 'cmd_delete_database_profile', ['delete_snapshots_in_folder']],
    ['backup.rs', 'cmd_delete_folder', ['delete_snapshots_in_folder']],
    ['backup.rs', 'cmd_delete_backup', ['delete_snapshot_with_lease']],
  ];
  for (const [file, name, calls] of cases) {
    const code = body(read(file), name);
    assert.equal((code.match(/begin_operation\(/g) || []).length, 1, name);
    for (const call of calls) {
      const occurrences = [...code.matchAll(new RegExp(call + '\\(\\s*&app,\\s*&engine,', 'g'))];
      assert.ok(occurrences.length, name + ' -> ' + call);
      assert.equal(occurrences.length, (code.match(new RegExp(call + '\\(', 'g')) || []).length);
    }
  }
});
