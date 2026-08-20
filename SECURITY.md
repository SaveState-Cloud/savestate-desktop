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
