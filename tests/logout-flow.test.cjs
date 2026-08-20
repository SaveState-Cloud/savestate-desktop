const test = require('node:test');
const assert = require('node:assert/strict');
const { confirmActiveBackups, warningMessage } = require('../src/logout-flow.js');

test('declining the active-backup warning leaves logout unconfirmed', async () => {
    let prompts = 0;
    const confirmed = await confirmActiveBackups(
        [{ id: 'one', name: 'Nightly database' }],
        async (message, options) => {
            prompts += 1;
            assert.match(message, /immediately stop this active backup/i);
            assert.match(message, /Nightly database/);
            assert.equal(options.kind, 'warning');
            return false;
        },
    );
    assert.equal(prompts, 1);
    assert.equal(confirmed, false);
});

test('no active backups needs no confirmation', async () => {
    const confirmed = await confirmActiveBackups([], async () => {
        throw new Error('confirmation should not be shown');
    });
    assert.equal(confirmed, true);
});

test('warning limits displayed names without hiding the total', () => {
    const backups = ['A', 'B', 'C', 'D'].map((name, index) => ({ id: String(index), name }));
    assert.match(warningMessage(backups), /all 4 active backups: A, B, C and 1 more/);
});
