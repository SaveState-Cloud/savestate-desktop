const test = require('node:test');
const assert = require('node:assert/strict');

const {
    customerVisibleUsage,
    shouldScheduleCleanup,
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
