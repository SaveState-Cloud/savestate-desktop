(function exposeVaultRecoveryHelpers(root, factory) {
    const helpers = factory();
    if (typeof module !== 'undefined' && module.exports) module.exports = helpers;
    if (root) root.SaveStateVaultRecovery = helpers;
})(typeof globalThis !== 'undefined' ? globalThis : this, function createVaultRecoveryHelpers() {
    'use strict';

    function classifyLoginResult(result) {
        const state = result && result.vaultState;
        if (state === 'recovery_key_ack_required') return 'setup';
        if (state === 'locked') return 'locked';
        if (state === 'ready' || state === 'ready_legacy') return 'ready';
        return 'invalid';
    }

    // The native command exposes a newly generated vault recovery key only in
    // its first setup response. Remove the property as soon as UI consumes it
    // so later application code cannot accidentally redisplay or log it.
    function consumeOneTimeVaultRecoveryKey(result) {
        if (!result || typeof result.vaultRecoveryKey !== 'string') return null;
        const key = result.vaultRecoveryKey;
        delete result.vaultRecoveryKey;
        return key;
    }

    function unlockOptions(result) {
        const options = [{ value: 'vault_password', label: 'Previous vault password' }];
        if (result && result.hasOfflineRecovery) {
            options.push({ value: 'vault_recovery_key', label: 'Offline vault recovery key' });
        }
        return options;
    }

    function clearSensitiveValue(element) {
        if (!element) return;
        if ('value' in element) element.value = '';
        element.textContent = '';
    }

    return {
        classifyLoginResult,
        consumeOneTimeVaultRecoveryKey,
        unlockOptions,
        clearSensitiveValue,
    };
});
