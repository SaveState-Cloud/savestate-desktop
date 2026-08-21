const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const root = path.resolve(__dirname, '..');
const html = fs.readFileSync(path.join(root, 'src', 'index.html'), 'utf8');
const app = fs.readFileSync(path.join(root, 'src', 'app.js'), 'utf8');

test('notification settings expose only a masked, write-only Discord webhook', () => {
  assert.match(html, /id="settings-webhook-url"[^>]+type="password"|type="password"[^>]+id="settings-webhook-url"/);
  assert.match(html, /id="btn-toggle-webhook-visibility"/);
  assert.match(html, /id="btn-remove-webhook"/);
  assert.match(app, /discordWebhookConfigured/);
  assert.match(app, /clearDiscordWebhook/);
});

test('Discord bot controls and channel identifiers are absent', () => {
  assert.doesNotMatch(html, /Discord Bot|Invite Bot|settings-channel-id|btn-invite-bot/);
  assert.doesNotMatch(app, /discordChannelId|settings-channel-id|btn-invite-bot/);
});
