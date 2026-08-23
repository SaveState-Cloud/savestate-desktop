const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

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
    }), {
        sourceBytes: 9_000_000,
        snapshotCount: 2,
        fileCount: 200,
    });
});

test('older API responses fall back to their visible usage without exposing empty overhead', () => {
    assert.deepEqual(sourceStatistics({ bytes: 2_000_000 }, { backups: [{ id: 'snapshot' }] }), {
        sourceBytes: 2_000_000,
        snapshotCount: null,
        fileCount: null,
    });
    assert.equal(sourceStatistics({ bytes: 2_000_000 }, { backups: [] }).sourceBytes, 0);
});

test('customer source statistics never derive or expose compression savings', () => {
    assert.deepEqual(sourceStatistics({
        bytes: 9_000_000,
        sourceBytes: 9_000_000,
        spaceSavedBytes: 7_000_000,
        savingsPercent: 77.78,
    }), {
        sourceBytes: 9_000_000,
        snapshotCount: null,
        fileCount: null,
    });
});

test('the customer dashboard presents only original backup data usage', () => {
    const html = fs.readFileSync(path.join(__dirname, '..', 'src', 'index.html'), 'utf8');
    const app = fs.readFileSync(path.join(__dirname, '..', 'src', 'app.js'), 'utf8');
    assert.match(html, /Backup data used/);
    assert.match(html, /original size of your retained backups/i);
    assert.doesNotMatch(html, /Storage saved/);
    assert.doesNotMatch(html, /compressed, deduplicated, and plan-limited/i);
    assert.doesNotMatch(app, /spaceSavedBytes|savingsPercent/);
    assert.match(app, /source_quota_exceeded/);
    assert.match(app, /exceed your plan’s original-data allowance/);
});
