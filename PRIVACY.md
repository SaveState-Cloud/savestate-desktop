# Desktop client privacy

This document describes network behavior in the open-source SaveState Desktop
client. It complements the service privacy policy presented on the SaveState
website.

## Services contacted by the client

The application communicates with `api.savestate.dk` over HTTPS to:

- authenticate the account and maintain a session;
- retrieve account, plan, usage, and backup metadata;
- obtain an account-scoped encrypted-repository session;
- report backup, restore, deletion, maintenance, and scheduled-job status;
- synchronize privacy-limited schedule timing metadata;
- manage folders, retention, notification settings, and subscription actions;
  and
- check for application updates.

File contents and repository metadata are encrypted on the user's device before
upload. The current hosted service proxies that encrypted repository traffic
through SaveState's ciphertext-only API gateway in the European Union to
Backblaze B2 storage in the European Union. Restore operations follow the
reverse route and are decrypted only on the user's device.

The gateway authorizes the repository request and forwards encrypted objects;
it does not receive the repository password, decrypted master key, or plaintext
backup contents. As described in `SECURITY.md`, account authentication and key
delivery still depend on the hosted SaveState API, so this is client-side
encryption rather than a claim of strict zero-knowledge authentication.

If a user configures Discord notifications, the destination is stored by the
SaveState API and delivery is performed by the hosted service. The desktop
client does not contact Discord directly.

## Account recovery and multi-factor authentication

For accounts that enable TOTP or use recovery, the hosted service processes the
account email address, an encrypted TOTP authenticator seed, and hashed
account-recovery tokens or recovery codes. It also retains limited
security-audit metadata and a one-way request fingerprint to rate-limit abuse
and investigate recovery activity. Recovery tokens and codes are not stored in
plaintext, and the TOTP seed is encrypted at rest.

Email access, a TOTP authenticator, and recovery codes authenticate control of
the SaveState account. They are not the vault master key and cannot themselves
decrypt a backup. Access to existing encrypted backups still depends on the
separately protected, client-owned master-key envelope or key slot. Account
recovery does not create a server-side plaintext copy of that key.

## Information intentionally excluded from job telemetry

Engine job events identify the authenticated installation and operation, but
the client is designed not to send source-folder paths, restored destination
paths, profile names, filenames, file contents, repository passwords, or the
decrypted master key as job telemetry.

## Third-party requests

The runtime UI uses operating-system fonts and does not load web fonts or
analytics scripts. The Kopia download in `scripts/bundle-kopia.mjs` occurs only
during a developer or CI build and is verified against a pinned SHA-256 hash.

## User control

Users can sign out to remove the remembered session, disable schedules, remove
notification settings, and request account management through the SaveState
service. Network access is required for cloud backup, restore, account, usage,
and update features.
