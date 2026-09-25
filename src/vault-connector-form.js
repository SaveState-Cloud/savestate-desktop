(function (root) {
    const connectors = {
        b2: {
            endpointLabel: 'Backblaze B2 endpoint',
            endpointPlaceholder: 'https://s3.eu-central-003.backblazeb2.com',
            endpointHelp: 'Use the S3 endpoint shown for your B2 bucket.',
            regionPlaceholder: 'e.g. eu-central-003',
            defaultRegion: 'eu-central-003',
        },
        r2: {
            endpointLabel: 'Cloudflare R2 endpoint',
            endpointPlaceholder: 'https://your-account-id.r2.cloudflarestorage.com',
            endpointHelp: 'Use your account’s S3 API endpoint, not a public bucket URL.',
            regionPlaceholder: 'auto',
            defaultRegion: 'auto',
        },
        s3: {
            endpointLabel: 'S3-compatible endpoint',
            endpointPlaceholder: 'https://storage.example.com',
            endpointHelp: 'Use your storage provider’s S3 API endpoint, not a public download URL.',
            regionPlaceholder: 'e.g. eu-central-1',
            defaultRegion: '',
        },
        minio: {
            endpointLabel: 'MinIO server URL',
            endpointPlaceholder: 'http://127.0.0.1:9000',
            endpointHelp: 'Local MinIO can use HTTP on this PC. Remote MinIO requires HTTPS.',
            regionPlaceholder: 'us-east-1',
            defaultRegion: 'us-east-1',
        },
    };

    function apply(document, { providerChanged = false } = {}) {
        const provider = document.getElementById('byos-provider').value;
        const connector = connectors[provider];
        if (!connector) throw new Error(`Unknown storage connector: ${provider}`);
        const endpoint = document.getElementById('byos-endpoint');
        const region = document.getElementById('byos-region');
        document.getElementById('byos-endpoint-label').textContent = connector.endpointLabel;
        document.getElementById('byos-endpoint-help').textContent = connector.endpointHelp;
        endpoint.placeholder = connector.endpointPlaceholder;
        region.placeholder = connector.regionPlaceholder;
        if (providerChanged) {
            endpoint.value = '';
            region.value = connector.defaultRegion;
        }
    }

    const api = { apply };
    if (typeof module !== 'undefined' && module.exports) module.exports = api;
    root.SaveStateVaultConnectorForm = api;
})(typeof window === 'undefined' ? globalThis : window);
