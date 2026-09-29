(function exposeStorageUsage(root, factory) {
    const api = factory();
    if (typeof module === 'object' && module.exports) module.exports = api;
    if (root) root.SaveStateStorageUsage = api;
})(typeof window !== 'undefined' ? window : globalThis, function createStorageUsage() {
    // Plans are measured from the encrypted repository footprint after Kopia
    // compression and deduplication. Original source bytes remain a separate
    // protection statistic so customers can see how much data is recoverable.
    function customerVisibleUsage(usage, backupState) {
        const optimizedBytes = optionalWholeNumber(usage?.bytes);
        if (optimizedBytes !== null) {
            // An empty repository still contains Kopia metadata. Hide only a
            // small footprint when the backup list was loaded successfully and
            // the server confirms that no files are retained. This is display
            // only; the API continues to meter the actual physical bytes.
            if (optimizedBytes < 5 * 1024 * 1024
                && Array.isArray(backupState?.backups)
                && optionalWholeNumber(usage?.fileCount) === 0) return 0;
            return optimizedBytes;
        }
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
