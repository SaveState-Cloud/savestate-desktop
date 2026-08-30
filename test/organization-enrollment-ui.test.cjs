const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');
const api = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'api.rs'), 'utf8');
const enrollment = fs.readFileSync(
  path.join(root, 'src-tauri', 'src', 'organization_enrollment.rs'),
  'utf8',
);

test('Settings requires review and explicit confirmation before connecting', () => {
  for (const id of [
    'organization-installation-section',
    'organization-setup-token',
    'btn-paste-organization-token',
    'btn-review-organization-enrollment',
    'organization-enrollment-preview',
    'organization-preview-name',
    'organization-preview-customer',
    'organization-preview-server',
    'organization-preview-expiry',
    'btn-confirm-organization-enrollment',
  ]) {
    assert.match(html, new RegExp(`id=["']${id}["']`));
  }
  assert.match(html, /type="password"[^>]+id="organization-setup-token"|id="organization-setup-token"[^>]+type="password"/);
  assert.match(html, /review the organization and server before anything changes/i);
  assert.match(html, /never receives your password, recovery keys, or decrypted backups/i);
  assert.match(app, /cmd_inspect_organization_installation/);
  assert.match(app, /navigator\.clipboard\.readText\(\)/);
  assert.match(app, /if \(!organizationEnrollmentPreview\)/);
  assert.match(app, /cmd_redeem_organization_installation/);
});

test('successful redemption switches the service session and warms the repository', () => {
  assert.match(app, /renderOrganizationInstallationStatus\(result\)/);
  assert.match(app, /warmRepositoryInBackground\(\)/);
  assert.match(api, /serviceId/);
  assert.match(api, /pub account_token: String/);
  assert.match(enrollment, /guard\.api\.set_token\(response\.account_token\.clone\(\)\)/);
  assert.match(enrollment, /session_generation\.wrapping_add\(1\)/);
  assert.match(enrollment, /clear_session_cache\(\)/);
});

test('the device credential is stored in Windows Credential Manager and never exposed to the UI', () => {
  assert.match(enrollment, /keyring::v1::Entry::new\("SaveState Vault", "organization-installation"\)/);
  assert.match(enrollment, /set_secret\(&data\)/);
  assert.match(enrollment, /delete_credential\(\)/);
  assert.match(enrollment, /invalid_device_credential/);
  assert.doesNotMatch(enrollment, /std::fs::write/);
  assert.doesNotMatch(html, /device[_ -]?credential/i);
  assert.doesNotMatch(app, /deviceCredential/);
  assert.doesNotMatch(enrollment, /pub device_credential/);
});

test('stable enrollment failures have short customer-facing messages', () => {
  for (const code of [
    'setup_token_expired',
    'setup_token_used',
    'customer_approval_required',
    'installation_disabled',
    'device_already_connected',
    'storage_service_unavailable',
  ]) {
    assert.match(app, new RegExp(code));
  }
});

test('the connected Windows app sends periodic and backup-outcome heartbeats', () => {
  const main = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'main.rs'), 'utf8');
  const kopia = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'kopia.rs'), 'utf8');
  assert.match(api, /organization\/installations\/heartbeat/);
  assert.match(main, /Duration::from_secs\(300\)/);
  assert.match(main, /send_organization_installation_heartbeat/);
  assert.match(kopia, /EngineJobReporter::start_backup/);
  assert.match(api, /matches!\(status, "succeeded" \| "failed"\)/);
  assert.match(api, /OrganizationBackupHeartbeat/);
  assert.match(enrollment, /pending_backup/);
  assert.match(enrollment, /retry_delays = \[0, 2, 10\]/);
  assert.doesNotMatch(api, /OrganizationBackupHeartbeat[\s\S]{0,500}(?:source_path|filename|database_name)/);
});
