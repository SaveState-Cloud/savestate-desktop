const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const {
    customerVisibleUsage,
    shouldScheduleCleanup,
    sourceStatistics,
} = require('../src/storage-usage.js');

test('confirmed empty backup lists still show the optimized repository footprint', () => {
    const backupState = { backups: [] };
    assert.equal(customerVisibleUsage({ bytes: 242_984_291 }, backupState), 242_984_291);
});

test('customer quota usage uses optimized storage while source bytes remain separate', () => {
    assert.equal(customerVisibleUsage({ bytes: 12_345, sourceBytes: 98_765 }, null), 12_345);
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

test('the customer dashboard presents optimized quota usage and source protection separately', () => {
    const html = fs.readFileSync(path.join(__dirname, '..', 'src', 'index.html'), 'utf8');
    const app = fs.readFileSync(path.join(__dirname, '..', 'src', 'app.js'), 'utf8');
    assert.match(html, /Storage used/);
    assert.match(html, /measured from encrypted storage after compression and deduplication/i);
    assert.match(html, /source data is shown separately/i);
    assert.match(html, /unlimited encrypted restore traffic · free/i);
    assert.match(app, /source data protected/i);
    assert.match(app, /source_quota_exceeded/);
    assert.match(app, /temporary safety ceiling/);
});
