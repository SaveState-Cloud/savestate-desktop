# Security policy

## Supported versions

Security updates are provided for the most recent released major version of
SaveState Desktop. Users should keep automatic updates enabled and upgrade to
the latest available release.

## Report a vulnerability privately

Do not open a public issue for a suspected vulnerability, exposed credential,
or method of accessing another customer's data.

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/SaveState-Cloud/savestate-desktop/security/advisories/new>

Include the affected version, reproduction steps, expected impact, and any
suggested remediation. Avoid accessing, downloading, or modifying data that
does not belong to you.

## Security boundary

The desktop client is responsible for:

- generating and handling the per-account backup master key;
- encrypting backup data before upload;
- storing remembered credentials in the operating system credential vault;
- accepting only authenticated account-scoped API responses; and
- using short-lived, account-scoped credentials for SaveState's encrypted
  repository gateway; Backblaze provider credentials do not reach the client.

The hosted SaveState services are responsible for authentication,
authorization, account ownership, quotas, billing, storage credential scope,
and telemetry access controls. A client-side check is never considered an
authorization boundary.

The account password is transmitted to the API over HTTPS for authentication.
The current design therefore provides client-side encryption but does not claim
that the hosted authentication service is cryptographically unable to observe
the password while processing a login.

## Release integrity

Release artifacts must be built from this public repository through the
documented CI and code-signing process. Updater signing keys and infrastructure
credentials must only exist in protected CI secret storage and must never be
committed to the repository.

Production Windows releases are created only by
`.github/workflows/release.yml` from a semantic-version tag contained in
`main`. The workflow builds the installers once, publishes the exact same MSI
and EXE bytes to GitHub Releases and the versioned R2 download path, and emits
SHA-256 checksums plus build metadata. Published version paths are immutable;
fixes use a new version rather than replacing an existing release.

### Interrupted release recovery

The workflow deliberately refuses to overwrite either an existing GitHub
Release or a non-empty versioned R2 prefix. If a run stops after creating its
draft release or after uploading only part of its R2 objects, do not rerun it
and do not rebuild that version.

An administrator may complete the interrupted release manually only when all
of the following are true:

1. the draft release's `release-provenance.json` names the same repository,
   tag, and commit as the immutable versioned R2 prefix;
2. every existing R2 object downloads to the SHA-256 value recorded in the
   draft release's `SHA256SUMS`;
3. any missing R2 object is copied from that same draft release without
   replacing an existing object; and
4. the latest manifests are published only after the MSI, EXE, and updater
   signature have all been verified.

The draft may then be published. Record the manual recovery in the repository's
security audit trail. If the provenance, hashes, or source commit cannot be
verified, leave that version unpublished and issue a new patch version; never
delete or replace artifacts in order to reuse the failed version number.
