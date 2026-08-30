const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const {
    customerVisibleUsage,
    shouldScheduleCleanup,
    sourceStatistics,
} = require('../src/storage-usage.js');

test('confirmed empty backup lists never expose repository overhead', () => {
    const backupState = { backups: [] };
    assert.equal(customerVisibleUsage({ bytes: 242_984_291 }, backupState), null);
});

test('customer usage uses original source bytes only', () => {
    assert.equal(customerVisibleUsage({ bytes: 12_345, sourceBytes: 98_765 }, null), 98_765);
});

test('original-source rollout responses remain customer-visible', () => {
    assert.equal(customerVisibleUsage(
        { bytes: 12_345, basis: 'original-source-bytes' },
        { backups: [{ id: 'snapshot' }] },
    ), 12_345);
});

test('only an explicit API pressure hint schedules maintenance', () => {
    const backupState = { backups: [] };
    assert.equal(shouldScheduleCleanup({ bytes: 1, maintenanceRecommended: true }, null), true);
    assert.equal(shouldScheduleCleanup({ bytes: 242_984_291 }, backupState), false);
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

test('the customer dashboard presents source-sized storage and never physical savings', () => {
    const html = fs.readFileSync(path.join(__dirname, '..', 'src', 'index.html'), 'utf8');
    const app = fs.readFileSync(path.join(__dirname, '..', 'src', 'app.js'), 'utf8');
    assert.match(html, /Storage used/);
    assert.match(html, /original size of data represented by retained restore points/i);
    assert.doesNotMatch(html, /compressed|deduplicated|Backup data protected/i);
    assert.match(html, /unlimited encrypted restore traffic · free/i);
    assert.doesNotMatch(app, /spaceSavedBytes|savingsPercent/);
    assert.match(app, /source_quota_exceeded/);
    assert.match(app, /temporary safety ceiling/);
});
