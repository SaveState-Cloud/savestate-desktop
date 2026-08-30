(function exposeStorageUsage(root, factory) {
    const api = factory();
    if (typeof module === 'object' && module.exports) module.exports = api;
    if (root) root.SaveStateStorageUsage = api;
})(typeof window !== 'undefined' ? window : globalThis, function createStorageUsage() {
    // Customers are measured only by the original data represented by their
    // retained restore points. Encrypted physical repository bytes are an
    // operator-only capacity and safety metric.
    function customerVisibleUsage(usage, backupState) {
        return sourceStatistics(usage, backupState).sourceBytes;
    }

    function shouldScheduleCleanup(usage, backupState) {
        void backupState;
        return usage?.maintenanceRecommended === true;
    }

    function optionalWholeNumber(value) {
        if (value === null || value === undefined || value === '') return null;
        const number = Number(value);
        return Number.isSafeInteger(number) && number >= 0 ? number : null;
    }

    function sourceStatistics(usage, backupState) {
        const reportedSourceBytes = optionalWholeNumber(usage?.sourceBytes);
        const legacySourceBytes = usage?.basis === 'original-source-bytes'
            ? optionalWholeNumber(usage?.bytes)
            : null;
        return {
            sourceBytes: reportedSourceBytes ?? legacySourceBytes,
            snapshotCount: optionalWholeNumber(usage?.snapshotCount),
            fileCount: optionalWholeNumber(usage?.fileCount),
        };
    }

    return { customerVisibleUsage, shouldScheduleCleanup, sourceStatistics };
});
