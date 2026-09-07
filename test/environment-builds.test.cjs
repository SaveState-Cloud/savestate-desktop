const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const test = require('node:test');

const packageJson = JSON.parse(readFileSync(join(__dirname, '..', 'package.json'), 'utf8'));
const development = JSON.parse(readFileSync(join(__dirname, '..', 'src-tauri', 'tauri.development.conf.json'), 'utf8'));
const staging = JSON.parse(readFileSync(join(__dirname, '..', 'src-tauri', 'tauri.staging.conf.json'), 'utf8'));
const production = JSON.parse(readFileSync(join(__dirname, '..', 'src-tauri', 'tauri.conf.json'), 'utf8'));
const apiSource = readFileSync(join(__dirname, '..', 'src-tauri', 'src', 'api.rs'), 'utf8');

test('development, staging, and production apps have isolated identities', () => {
  assert.equal(development.identifier, 'dk.savestate.vault.development');
  assert.equal(staging.identifier, 'dk.savestate.vault.staging');
  assert.equal(production.identifier, 'dk.savestate.vault');
  assert.notEqual(development.productName, staging.productName);
  assert.notEqual(staging.productName, production.productName);
});

test('environment builds compile only their matching API hostname', () => {
  assert.match(packageJson.scripts['build:development'], /SAVESTATE_API_BASE_URL=https:\/\/api-dev\.savestate\.dk/);
  assert.match(packageJson.scripts['build:staging'], /SAVESTATE_API_BASE_URL=https:\/\/api-staging\.savestate\.dk/);
  assert.match(development.app.security.csp, /https:\/\/api-dev\.savestate\.dk/);
  assert.doesNotMatch(development.app.security.csp, /https:\/\/api\.savestate\.dk(?:\s|;)/);
  assert.match(staging.app.security.csp, /https:\/\/api-staging\.savestate\.dk/);
  assert.match(apiSource, /option_env!\("SAVESTATE_API_BASE_URL"\)/);
});

test('internal app updaters cannot install from the production update endpoint', () => {
  assert.match(development.plugins.updater.endpoints[0], /^https:\/\/api-dev\.savestate\.dk\//);
  assert.match(staging.plugins.updater.endpoints[0], /^https:\/\/api-staging\.savestate\.dk\//);
  assert.match(production.plugins.updater.endpoints[0], /^https:\/\/api\.savestate\.dk\//);
});

test('environment builds isolate all local data and secure credentials', () => {
  const source = name => readFileSync(join(__dirname, '..', 'src-tauri', 'src', name), 'utf8');
  for (const name of ['main.rs', 'auth.rs', 'kopia.rs']) {
    assert.match(source(name), /runtime_storage::data_dir\(\)/);
    assert.doesNotMatch(source(name), /join\("SaveState"\)/);
  }
  for (const name of ['auth.rs', 'databases.rs', 'organization_enrollment.rs']) {
    assert.match(source(name), /runtime_storage::credential_service\(/);
  }
  assert.match(source('main.rs'), /if runtime_storage::is_production\(\)\s*\{\s*if let Err\(error\) =\s*initialize_windows_autostart/);
});
