const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const styles = fs.readFileSync(path.join(root, 'src', 'styles.css'), 'utf8');
const main = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'main.rs'), 'utf8');
const managed = fs.readFileSync(
  path.join(root, 'src-tauri', 'src', 'managed_device.rs'),
  'utf8',
);
const api = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'api.rs'), 'utf8');
const enrollment = fs.readFileSync(
  path.join(root, 'src-tauri', 'src', 'organization_enrollment.rs'),
  'utf8',
).replace(/\r\n/g, '\n');

test('both enrollment paths require explicit consent for every administrator control', () => {
  for (const id of [
    'organization-account-control-consent',
    'organization-token-control-consent',
  ]) {
    assert.match(html, new RegExp(`type="checkbox"[^>]+id="${id}"|id="${id}"[^>]+type="checkbox"`));
    assert.match(app, new RegExp(`getElementById\\('${id}'\\)\\.checked`));
  }
  for (const phrase of [
    'select any accessible local folder path on this PC and see that source path metadata',
    'create, pause, and remove its managed backup schedules',
    'start and cancel backups',
    'restore an entire snapshot into a new folder inside SaveState Restores',
    'delete listed snapshots on this PC',
    'Network shares and mapped drives cannot be used',
    'never receive my plaintext encryption keys or decrypted backup contents',
  ]) {
    assert.match(html, new RegExp(phrase, 'i'));
  }
  assert.match(html, /id="btn-connect-account-organization" disabled/);
  assert.match(html, /id="btn-confirm-organization-enrollment" disabled/);
  assert.match(app, /Confirm the organization administrator controls before connecting this PC/);
  assert.match(app, /Confirm the organization administrator controls before connecting this installation/);
});

test('organization policies are visibly owned and locally read-only', () => {
  assert.match(app, /invoke\('cmd_list_managed_profiles'\)/);
  assert.match(app, /managed-profile-card/);
  assert.match(app, /Managed by \$\{escapeHtml\(organizationName\)\}/);
  assert.match(app, /Schedule, source, and retention are read-only here/);
  assert.match(app, /data-managed-assignment-id/);
  assert.match(styles, /\.managed-profile-owner/);
  assert.match(styles, /\.managed-profile-lock/);

  const managedCards = app.slice(
    app.indexOf('(managedProfiles || []).forEach'),
    app.indexOf('(profiles || []).forEach'),
  );
  assert.match(managedCards, /Open Backups/);
  assert.doesNotMatch(managedCards, /cmd_run_profile_backup/);
  assert.doesNotMatch(managedCards, /openProfileModal/);
  assert.doesNotMatch(managedCards, /openProfileDeleteModal/);
});

test('the Windows agent exposes only fixed versioned control routes and typed commands', () => {
  const managedRuntime = managed.slice(0, managed.indexOf('#[cfg(test)]'));
  for (const route of [
    'organization/installations/control/poll',
    'organization/installations/control/ack',
    'organization/installations/control/events',
  ]) {
    assert.match(api, new RegExp(route.replaceAll('/', '\\/')));
  }
  for (const kind of [
    'policy_sync',
    'run_backup',
    'cancel_backup',
    'restore_snapshot',
    'delete_snapshot',
    'pause',
    'resume',
  ]) {
    assert.match(managed, new RegExp(`"${kind}"`));
  }
  assert.match(managed, /ORGANIZATION_CONTROL_SCHEMA_VERSION/);
  assert.match(managed, /deny_unknown_fields/);
  assert.match(managed, /state_generation: i64/);
  assert.match(managed, /"appliedStateGeneration"/);
  assert.match(managed, /"policy_state_generation"|policy_state_generation/);
  assert.match(managed, /"deletionReason"/);
  assert.match(managed, /Some\("administrator"\)/);
  assert.match(managed, /Some\("retention"\)/);
  assert.doesNotMatch(managedRuntime, /(?:cmd\.exe|powershell|Command::new|std::process::Command)/i);
});

test('managed polling, local scheduling, and restart reconciliation are wired at startup', () => {
  assert.match(main, /managed_device::init_db/);
  assert.match(main, /managed_device::reconcile_startup/);
  assert.match(main, /managed_device::control_tick/);
  assert.match(main, /managed_device::schedule_tick/);
  assert.match(main, /cmd_list_managed_profiles/);
  assert.match(managed, /CONTROL_IDLE_INTERVAL_SECONDS: u64 = 30/);
  assert.match(managed, /managed_command_receipts/);
  assert.match(managed, /managed_control_events/);
  assert.match(managed, /managed_backup_policies/);
  assert.match(managed, /managed_jobs/);
  assert.match(managed, /cleanup_if_device_credential_revoked/);
  assert.match(managed, /flush_pending_events/);
  assert.match(managed, /flush_pending_acks/);
  assert.match(managed, /create_protected_managed_restore_directory/);
  assert.match(managed, /open_pinned_local_ancestors/);
  assert.match(managed, /SetFileInformationByHandle/);
});

test('managed restore destinations are generated in a protected app-owned root and shown locally only', () => {
  const payload = managed.slice(managed.indexOf('struct RestoreSnapshotPayload'), managed.indexOf('struct DeleteSnapshotPayload'));
  assert.match(payload, /schema_version: u32/);
  assert.match(payload, /snapshot_id: String/);
  assert.doesNotMatch(payload, /destination_path|overwrite/);
  assert.match(managed, /data_dir\(\)\.join\("SaveState Restores"\)/);
  assert.match(managed, /managed_restore_destination_candidate/);
  assert.match(managed, /safe_generated_restore_leaf/);
  assert.match(managed, /destination_wide\.push\(0\)/);
  assert.match(managed, /FileNameLength = file_name_bytes as u32/);
  assert.match(managed, /protect_existing_managed_restore_directory/);
  const kopia = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');
  assert.match(kopia, /trigger == "managed_restore"/);
  assert.match(kopia, /--skip-owners/);
  assert.match(kopia, /--skip-permissions/);
  assert.match(main, /cmd_list_local_managed_restores/);
  assert.match(app, /invoke\('cmd_list_local_managed_restores'\)/);
  assert.match(app, /destination\.textContent = restore\.destination_path/);
  assert.match(html, /actual destination paths are shown only on this PC/);
});

test('Settings disconnects only after a snapshot-preserving managed-work warning', () => {
  assert.match(html, /id="btn-disconnect-organization-installation"/);
  assert.match(app, /Managed schedules will stop and queued or running managed work will be cancelled/);
  assert.match(app, /Existing snapshots will remain available, and personal profiles will not be changed/);
  assert.match(app, /invoke\('cmd_disconnect_organization_installation'\)/);
  assert.match(main, /cmd_disconnect_organization_installation/);
  assert.match(api, /organization\/installations\/disconnect/);
  assert.match(api, /"requestId": request_id/);
  assert.match(managed, /managed_disconnect_state/);
  assert.match(managed, /disconnect_local_managed_state/);

  const start = enrollment.indexOf('async fn disconnect_organization_installation(');
  const end = enrollment.indexOf('\n}\n', start);
  assert.ok(start >= 0 && end > start, 'native disconnect function is missing');
  const body = enrollment.slice(start, end);
  const request = body.indexOf('.disconnect_organization_installation(');
  const localCleanup = body.indexOf('disconnect_local_managed_state(');
  const removeCredential = body.indexOf('INSTALLATIONS.remove_if_current(&stored)');
  assert.ok(request >= 0 && request < localCleanup && localCleanup < removeCredential);
  assert.match(enrollment, /cleanup_revoked_managed_context/);
  assert.match(enrollment, /cleanup_revoked_installation\(state, &stored\)\.await/);
});
