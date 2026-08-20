const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {
    classifyLoginResult,
    consumeOneTimeVaultRecoveryKey,
    unlockOptions,
    clearSensitiveValue,
} = require('../src/vault-recovery.js');

test('classifies native vault states without treating account login as vault unlock', () => {
    assert.equal(classifyLoginResult({ vaultState: 'ready' }), 'ready');
    assert.equal(classifyLoginResult({ vaultState: 'locked' }), 'locked');
    assert.equal(classifyLoginResult({ vaultState: 'recovery_key_ack_required' }), 'setup');
    assert.equal(classifyLoginResult({ success: true }), 'invalid');
});

test('consumes the one-time vault recovery key and cannot return it twice', () => {
    const result = { vaultRecoveryKey: '256-bit-secret' };
    assert.equal(consumeOneTimeVaultRecoveryKey(result), '256-bit-secret');
    assert.equal(consumeOneTimeVaultRecoveryKey(result), null);
    assert.equal('vaultRecoveryKey' in result, false);
});

test('legacy vault does not offer an offline factor it never had', () => {
    assert.deepEqual(unlockOptions({ hasOfflineRecovery: false }), [
        { value: 'vault_password', label: 'Previous vault password' },
    ]);
    assert.equal(unlockOptions({ hasOfflineRecovery: true }).length, 2);
});

test('clears copied recovery material from text and input elements', () => {
    const element = { value: 'secret', textContent: 'secret' };
    clearSensitiveValue(element);
    assert.equal(element.value, '');
    assert.equal(element.textContent, '');
});

test('desktop markup contains every recovery control wired by the app', () => {
    const html = fs.readFileSync(path.join(__dirname, '..', 'src', 'index.html'), 'utf8');
    for (const id of [
        'vault-recovery-setup',
        'vault-recovery-key',
        'vault-recovery-ack',
        'vault-locked-card',
        'vault-unlock-method',
        'vault-unlock-secret',
        'vault-current-account-password',
    ]) {
        assert.match(html, new RegExp(`id=["']${id}["']`));
    }
    assert.match(html, /This is not an account recovery code/);
    assert.match(html, /cannot decrypt your backups/);
});
