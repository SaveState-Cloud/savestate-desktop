(function exposeLogoutFlow(root) {
    function warningMessage(activeBackups) {
        const names = activeBackups.map((backup) => String(backup?.name || 'Backup')).slice(0, 3);
        const extra = activeBackups.length > names.length
            ? ` and ${activeBackups.length - names.length} more`
            : '';
        const subject = activeBackups.length === 1
            ? 'this active backup'
            : `all ${activeBackups.length} active backups`;
        return `Signing out will immediately stop ${subject}: ${names.join(', ')}${extra}. Cancelled backups will not appear in My Backups. Do you want to stop them and sign out?`;
    }

    async function confirmActiveBackups(activeBackups, confirmDialog) {
        if (!Array.isArray(activeBackups) || activeBackups.length === 0) return true;
        return confirmDialog(warningMessage(activeBackups), {
            title: 'Stop active backups and sign out?',
            kind: 'warning',
        });
    }

    const api = { confirmActiveBackups, warningMessage };
    root.SaveStateLogout = api;
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
}(typeof window === 'undefined' ? globalThis : window));
