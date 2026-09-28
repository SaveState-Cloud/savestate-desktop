// A managed vault is a permanent UI destination. Native profiles store it as
// vault_id = NULL; customer-owned vaults keep their existing native IDs.
(function (root) {
    const MANAGED_VAULT_ID = 'cloud-personal';

    function profileVaultId(profile) {
        return profile?.vault_id || MANAGED_VAULT_ID;
    }

    function profilesInVault(profiles, vaultId) {
        return (profiles || []).filter(profile => profileVaultId(profile) === vaultId);
    }

    function vaultsWithCounts(customVaults, profiles, databaseProfiles = []) {
        const managed = {
            id: MANAGED_VAULT_ID,
            label: 'Cloud - Personal',
            provider: 'savestate',
            managed: true,
        };
        return [managed, ...(customVaults || []).map(vault => ({ ...vault, managed: false }))]
            .map(vault => {
                const sources = profilesInVault(profiles, vault.id);
                const databases = vault.managed ? databaseProfiles || [] : [];
                return {
                    ...vault,
                    sourceCount: sources.length,
                    databaseCount: databases.length,
                    scheduledCount: [...sources, ...databases]
                        .filter(profile => profile.enabled && String(profile.schedule || '').trim()).length,
                };
            });
    }

    const api = { MANAGED_VAULT_ID, profileVaultId, profilesInVault, vaultsWithCounts };
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
    root.SaveStateVaultModel = api;
})(typeof window === 'undefined' ? globalThis : window);
