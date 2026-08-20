(function exposeStorageUsage(root, factory) {
    const api = factory();
    if (typeof module === 'object' && module.exports) module.exports = api;
    if (root) root.SaveStateStorageUsage = api;
})(typeof window !== 'undefined' ? window : globalThis, function createStorageUsage() {
    function normalizeBytes(value) {
        const bytes = Number(value);
        return Number.isSafeInteger(bytes) && bytes >= 0 ? bytes : 0;
    }

    function knownBackupCount(backupState) {
        if (Array.isArray(backupState?.backups)) return backupState.backups.length;
        const count = Number(backupState?.count);
        return Number.isSafeInteger(count) && count >= 0 ? count : null;
    }

    // Older API versions reported physical Kopia repository overhead. Keep the
    // desktop honest during rollout by treating a confirmed empty manifest as
    // zero customer storage regardless of that internal footprint.
    function customerVisibleUsage(reportedBytes, backupState) {
        const bytes = normalizeBytes(reportedBytes);
        return knownBackupCount(backupState) === 0 ? 0 : bytes;
    }

    function shouldScheduleCleanup(reportedBytes, backupState) {
        return knownBackupCount(backupState) === 0 && normalizeBytes(reportedBytes) >= 1024 * 1024;
    }

    return { customerVisibleUsage, shouldScheduleCleanup };
});
