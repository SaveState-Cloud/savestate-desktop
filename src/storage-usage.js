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

    // Customer quota is the exact encrypted Kopia repository footprint. This
    // intentionally includes packs, indexes, metadata, and stored versions.
    function customerVisibleUsage(reportedBytes, backupState) {
        void backupState;
        return normalizeBytes(reportedBytes);
    }

    function shouldScheduleCleanup(usage, backupState) {
        if (usage?.maintenanceRecommended === true) return true;
        // A legacy API cannot send pressure hints. Cleanup confirmed empty
        // repositories with non-trivial overhead during the rolling update.
        return knownBackupCount(backupState) === 0
            && normalizeBytes(usage?.bytes ?? usage) >= 1024 * 1024;
    }

    function optionalWholeNumber(value) {
        if (value === null || value === undefined || value === '') return null;
        const number = Number(value);
        return Number.isSafeInteger(number) && number >= 0 ? number : null;
    }

    function sourceStatistics(usage, backupState) {
        const reportedSourceBytes = optionalWholeNumber(usage?.sourceBytes);
        const legacySourceBytes = usage?.basis === 'original-source-bytes'
            ? customerVisibleUsage(usage?.bytes, backupState)
            : null;
        return {
            sourceBytes: reportedSourceBytes ?? legacySourceBytes,
            snapshotCount: optionalWholeNumber(usage?.snapshotCount),
            fileCount: optionalWholeNumber(usage?.fileCount),
        };
    }

    function savingsStatistics(usage, sourceBytes, storageBytes) {
        const reportedSaved = optionalWholeNumber(usage?.spaceSavedBytes);
        const savedBytes = reportedSaved ?? (sourceBytes === null
            ? null
            : Math.max(0, sourceBytes - storageBytes));
        const reportedPercent = Number(usage?.savingsPercent);
        const savingsPercent = Number.isFinite(reportedPercent) && reportedPercent >= 0
            ? reportedPercent
            : sourceBytes && savedBytes !== null
                ? Number(((savedBytes / sourceBytes) * 100).toFixed(2))
                : sourceBytes === 0 ? 0 : null;
        return { savedBytes, savingsPercent };
    }

    return { customerVisibleUsage, shouldScheduleCleanup, sourceStatistics, savingsStatistics };
});
