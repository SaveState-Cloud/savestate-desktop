const test = require('node:test');
const assert = require('node:assert/strict');

const {
    customerVisibleUsage,
    shouldScheduleCleanup,
    sourceStatistics,
} = require('../src/storage-usage.js');

test('confirmed empty backup lists never display repository overhead', () => {
    const backupState = { backups: [] };
    assert.equal(customerVisibleUsage(242_984_291, backupState), 0);
});

test('unknown backup state does not hide reported customer usage', () => {
    assert.equal(customerVisibleUsage(12_345, null), 12_345);
});

test('existing backups preserve API-calculated customer usage', () => {
    assert.equal(customerVisibleUsage(12_345, { backups: [{ id: 'snapshot' }] }), 12_345);
});

test('legacy physical overhead schedules maintenance without changing display', () => {
    const backupState = { backups: [] };
    assert.equal(shouldScheduleCleanup(242_984_291, backupState), true);
    assert.equal(shouldScheduleCleanup(0, backupState), false);
});

test('source statistics preserve original bytes and retained backup counts', () => {
    assert.deepEqual(sourceStatistics({
        bytes: 2_000_000,
        sourceBytes: 9_000_000,
        snapshotCount: 2,
        fileCount: 200,
        spaceSavedBytes: 7_000_000,
        savingsPercent: 77.78,
    }), {
        sourceBytes: 9_000_000,
        snapshotCount: 2,
        fileCount: 200,
        spaceSavedBytes: 7_000_000,
        savingsPercent: 77.78,
    });
});

test('source statistics remain unavailable with an older API response', () => {
    assert.deepEqual(sourceStatistics({ bytes: 2_000_000 }), {
        sourceBytes: null,
        snapshotCount: null,
        fileCount: null,
        spaceSavedBytes: null,
        savingsPercent: null,
    });
});
