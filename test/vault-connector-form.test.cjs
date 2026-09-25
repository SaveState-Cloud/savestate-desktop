const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const connectorForm = require('../src/vault-connector-form.js');

function form(provider) {
  const elements = Object.fromEntries([
    'byos-provider', 'byos-endpoint', 'byos-region',
    'byos-endpoint-label', 'byos-endpoint-help',
    'byos-folder-picker', 'byos-storage-note',
    'byos-bucket', 'byos-prefix', 'byos-key-id', 'byos-secret',
    'byos-region-field', 'byos-bucket-field', 'byos-prefix-field',
    'byos-key-id-field', 'byos-secret-field',
  ].map(id => [id, { value: '', placeholder: '', textContent: '', classList: { hidden: false, toggle(name, value) { if (name === 'hidden') this.hidden = value; } } }]));
  elements['byos-provider'].value = provider;
  elements['byos-endpoint'].value = 'https://previous-provider.example.com';
  elements['byos-region'].value = 'previous-region';
  return { elements, document: { getElementById: id => elements[id] } };
}

test('each connector has its own endpoint label, example, help, and region', () => {
  const expected = {
    b2: ['Backblaze B2 endpoint', 'https://s3.eu-central-003.backblazeb2.com', 'eu-central-003'],
    r2: ['Cloudflare R2 endpoint', 'https://your-account-id.r2.cloudflarestorage.com', 'auto'],
    s3: ['S3-compatible endpoint', 'https://storage.example.com', ''],
    minio: ['MinIO server URL', 'http://127.0.0.1:9000', 'us-east-1'],
  };
  for (const [provider, [label, endpointExample, region]] of Object.entries(expected)) {
    const { elements, document } = form(provider);
    connectorForm.apply(document, { providerChanged: true });
    assert.equal(elements['byos-endpoint-label'].textContent, label);
    assert.equal(elements['byos-endpoint'].placeholder, endpointExample);
    assert.ok(elements['byos-endpoint-help'].textContent.length > 0);
    assert.equal(elements['byos-endpoint'].value, '', `${provider} must not reuse another provider’s endpoint`);
    assert.equal(elements['byos-region'].value, region);
  }
});

test('reopening a form keeps unfinished input', () => {
  const { elements, document } = form('minio');
  connectorForm.apply(document);
  assert.equal(elements['byos-endpoint'].value, 'https://previous-provider.example.com');
  assert.equal(elements['byos-region'].value, 'previous-region');
});

test('external drive uses a folder picker without cloud-only fields or keys', () => {
  const { elements, document } = form('filesystem');
  connectorForm.apply(document, { providerChanged: true });
  assert.equal(elements['byos-endpoint-label'].textContent, 'Vault folder on external drive');
  assert.equal(elements['byos-endpoint'].type, 'text');
  assert.equal(elements['byos-endpoint'].readOnly, true);
  assert.equal(elements['byos-folder-picker'].classList.hidden, false);
  for (const id of ['region', 'bucket', 'prefix', 'key-id', 'secret']) {
    assert.equal(elements[`byos-${id}-field`].classList.hidden, true);
  }
  assert.equal(elements['byos-secret'].required, false);
  assert.match(elements['byos-storage-note'].textContent, /never formats or erases/);
});

test('the form loads provider copy before the app and links help to the endpoint', () => {
  const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
  const app = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
  assert.ok(html.indexOf('src="vault-connector-form.js"') < html.indexOf('src="app.js"'));
  assert.match(html, /id="byos-endpoint"[^>]+aria-describedby="byos-endpoint-help"/);
  assert.match(app, /vaultConnectorForm\.apply\(document, \{ providerChanged: true \}\)/);
});
