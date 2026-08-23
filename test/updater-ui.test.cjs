const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');

function createClassList(initial = []) {
  const values = new Set(initial);
  return {
    add: (...names) => names.forEach((name) => values.add(name)),
    remove: (...names) => names.forEach((name) => values.delete(name)),
    contains: (name) => values.has(name),
    toggle: (name, force) => {
      const next = force === undefined ? !values.has(name) : force;
      if (next) values.add(name);
      else values.delete(name);
      return next;
    },
  };
}

function createUpdaterHarness(updateVersion = '9.9.9') {
  const elements = new Map();
  const getElement = (id) => {
    if (!elements.has(id)) {
      elements.set(id, {
        id,
        classList: createClassList(id === 'update-banner' || id === 'btn-settings-install-update' ? ['hidden'] : []),
        style: {},
        textContent: '',
        value: '',
        disabled: false,
        checked: false,
        addEventListener() {},
        querySelector() { return getElement(`${id}-child`); },
        querySelectorAll() { return []; },
      });
    }
    return elements.get(id);
  };

  const document = {
    addEventListener() {},
    getElementById: getElement,
    querySelectorAll() { return []; },
    createElement() { return getElement('created-element'); },
  };
  const window = {
    document,
    addEventListener() {},
    SaveStateVaultRecovery: {},
    SaveStateStorageUsage: {},
    __TAURI__: {
      app: { getVersion: async () => '2.0.21' },
      core: { invoke: async () => null },
      dialog: { open: async () => null, confirm: async () => false },
      event: { listen: async () => () => {} },
      process: { relaunch: async () => {} },
      updater: {
        check: async () => updateVersion ? { version: updateVersion, close: async () => {} } : null,
      },
    },
  };
  const context = vm.createContext({
    console,
    document,
    window,
    setInterval() { return 1; },
    setTimeout() { return 1; },
    clearTimeout() {},
  });
  vm.runInContext(app, context);
  return { context, getElement };
}

test('Settings exposes an update action only when an update is available', () => {
  assert.match(html, /id="settings-update-status"[^>]+role="status"/);
  assert.match(html, /id="settings-current-version"/);
  assert.match(html, /class="[^"]*hidden[^"]*"[^>]+id="btn-settings-install-update"|id="btn-settings-install-update"[^>]+class="[^"]*hidden/);
  assert.match(html, /id="btn-check-updates"/);
  assert.match(app, /settingsInstallButton\.classList\.toggle\('hidden', !updateAvailable\)/);
  assert.match(app, /Update to v\$\{availableUpdateVersion\}/);
});

test('launch checks reveal the update banner again in each new app session', () => {
  assert.match(app, /void checkForUpdates\(\{ revealAvailable: true \}\)/);
  assert.match(app, /let dismissedUpdateVersion = null/);
  assert.doesNotMatch(app, /localStorage|sessionStorage/);
  assert.match(html, /id="update-banner"[^>]+role="region"/);
  assert.match(html, /id="update-banner-text"[^>]+aria-live="polite"/);
});

test('a fresh app session reveals the banner and Settings update action', async () => {
  for (let session = 0; session < 2; session += 1) {
    const harness = createUpdaterHarness();
    await vm.runInContext('checkForUpdates({ revealAvailable: true })', harness.context);

    assert.equal(harness.getElement('update-banner').classList.contains('hidden'), false);
    assert.equal(harness.getElement('btn-settings-install-update').classList.contains('hidden'), false);
    assert.equal(harness.getElement('btn-settings-install-update').textContent, 'Update to v9.9.9');
    assert.match(harness.getElement('settings-update-status').textContent, /Version 9\.9\.9 is ready/);
  }
});

test('an up-to-date app keeps the Settings update action hidden', async () => {
  const harness = createUpdaterHarness(null);
  await vm.runInContext('loadCurrentAppVersion()', harness.context);
  await vm.runInContext('checkForUpdates({ revealAvailable: true })', harness.context);

  assert.equal(harness.getElement('update-banner').classList.contains('hidden'), true);
  assert.equal(harness.getElement('btn-settings-install-update').classList.contains('hidden'), true);
  assert.equal(harness.getElement('settings-update-status').textContent, "You're up to date on version 2.0.21.");
});

test('Settings and the banner share the same safe updater command', () => {
  assert.match(app, /btn-settings-install-update'[\s\S]+installUpdate\(\)/);
  assert.match(app, /btn-install-update'[\s\S]+installUpdate\(\)/);
  assert.match(app, /await invoke\('cmd_install_update'\)/);
  assert.match(app, /Updating waits until backups and restores are idle/);
});
