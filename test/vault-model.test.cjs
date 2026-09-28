const assert = require('node:assert/strict');
const test = require('node:test');
const vaults = require('../src/vault-model.js');

test('Cloud - Personal is always the first vault and keeps legacy null destinations', () => {
  const result = vaults.vaultsWithCounts([{ id: 'b2-1', label: 'Office', provider: 'b2' }], [
    { id: 'old', vault_id: null, enabled: true, schedule: '{"times":["14:15"]}' },
    { id: 'own', vault_id: 'b2-1', enabled: true, schedule: null },
  ]);
  assert.equal(result[0].id, vaults.MANAGED_VAULT_ID);
  assert.equal(result[0].label, 'Cloud - Personal');
  assert.equal(result[0].sourceCount, 1);
  assert.equal(result[0].databaseCount, 0);
  assert.equal(result[0].scheduledCount, 1);
  assert.equal(result[1].sourceCount, 1);
  assert.equal(result[1].scheduledCount, 0);
  assert.deepEqual(vaults.profilesInVault([{ vault_id: null }, { vault_id: 'b2-1' }], 'b2-1'), [{ vault_id: 'b2-1' }]);
});

test('disabled or manual-only sources do not consume the scheduled count', () => {
  const result = vaults.vaultsWithCounts([], [
    { vault_id: null, enabled: false, schedule: '{"times":["10:00"]}' },
    { vault_id: null, enabled: true, schedule: '' },
  ]);
  assert.equal(result.length, 1);
  assert.equal(result[0].sourceCount, 2);
  assert.equal(result[0].scheduledCount, 0);
});

test('a custom vault holds multiple scheduled backup sources', () => {
  const profiles = [
    { id: 'documents', vault_id: 'b2-1', enabled: true, schedule: '{"times":["09:00"]}' },
    { id: 'photos', vault_id: 'b2-1', enabled: true, schedule: '{"times":["18:00"]}' },
    { id: 'cloud', vault_id: null, enabled: true, schedule: '{"times":["12:00"]}' },
  ];
  const result = vaults.vaultsWithCounts([{ id: 'b2-1', label: 'Office', provider: 'b2' }], profiles);
  assert.equal(result[1].sourceCount, 2);
  assert.equal(result[1].scheduledCount, 2);
  assert.deepEqual(vaults.profilesInVault(profiles, 'b2-1').map(profile => profile.id), ['documents', 'photos']);
});

test('managed vault counts database backups without assigning them to custom storage', () => {
  const result = vaults.vaultsWithCounts(
    [{ id: 'r2-1', label: 'R2', provider: 'r2' }],
    [{ vault_id: 'r2-1', enabled: true, schedule: '{"times":["09:00"]}' }],
    [{ enabled: true, schedule: '{"times":["17:00"]}' }],
  );
  assert.equal(result[0].sourceCount, 0);
  assert.equal(result[0].databaseCount, 1);
  assert.equal(result[0].scheduledCount, 1);
  assert.equal(result[1].sourceCount, 1);
  assert.equal(result[1].scheduledCount, 1);
});
