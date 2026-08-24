const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const {
    customerVisibleUsage,
    savingsStatistics,
    shouldScheduleCleanup,
    sourceStatistics,
} = require('../src/storage-usage.js');

test('confirmed empty backup lists still count real repository overhead', () => {
    const backupState = { backups: [] };
    assert.equal(customerVisibleUsage(242_984_291, backupState), 242_984_291);
});

test('unknown backup state does not hide reported customer usage', () => {
    assert.equal(customerVisibleUsage(12_345, null), 12_345);
});

test('existing backups preserve API-calculated customer usage', () => {
    assert.equal(customerVisibleUsage(12_345, { backups: [{ id: 'snapshot' }] }), 12_345);
});

test('API pressure hints and legacy empty overhead schedule maintenance', () => {
    const backupState = { backups: [] };
    assert.equal(shouldScheduleCleanup({ bytes: 1, maintenanceRecommended: true }, null), true);
    assert.equal(shouldScheduleCleanup({ bytes: 242_984_291 }, backupState), true);
    assert.equal(shouldScheduleCleanup({ bytes: 0 }, backupState), false);
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

test('older original-source API responses remain compatible during rollout', () => {
    assert.deepEqual(sourceStatistics({ bytes: 2_000_000, basis: 'original-source-bytes' }, { backups: [{ id: 'snapshot' }] }), {
        sourceBytes: 2_000_000,
        snapshotCount: null,
        fileCount: null,
    });
    assert.equal(sourceStatistics({ bytes: 2_000_000 }, { backups: [] }).sourceBytes, null);
});

test('customer statistics expose compression and deduplication savings', () => {
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
    assert.deepEqual(savingsStatistics({
        spaceSavedBytes: 91_000_000,
        savingsPercent: 91,
    }, 100_000_000, 9_000_000), {
        savedBytes: 91_000_000,
        savingsPercent: 91,
    });
});

test('the customer dashboard presents optimized storage, protected data, savings, and free restores', () => {
    const html = fs.readFileSync(path.join(__dirname, '..', 'src', 'index.html'), 'utf8');
    const app = fs.readFileSync(path.join(__dirname, '..', 'src', 'app.js'), 'utf8');
    assert.match(html, /Storage used/);
    assert.match(html, /compressed and deduplicated encrypted repository footprint/i);
    assert.match(html, /Backup data protected/);
    assert.match(html, /unlimited encrypted restore traffic · free/i);
    assert.match(app, /spaceSavedBytes|savingsPercent/);
    assert.match(app, /source_quota_exceeded/);
    assert.match(app, /temporary safety ceiling/);
});
