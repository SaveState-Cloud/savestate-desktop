# SaveState Desktop

SaveState Desktop is the official open-source Windows client for
[SaveState](https://savestate.dk), an encrypted cloud backup service. The app
configures scheduled backups, creates and restores encrypted Kopia snapshots,
and reports privacy-limited job status to the SaveState service.

## Public and private components

This repository contains only the software that runs on a customer's Windows
device:

- the Tauri desktop shell and user interface;
- local scheduling and profile management;
- client-side key handling and encryption;
- the integration with the open-source Kopia backup engine; and
- the authenticated client for the public SaveState API.

The hosted API, operator Engine, storage infrastructure, billing systems,
website, and future business, reseller, and affiliate panels are separate
services and are not included here.

## Security model

- Backup data is encrypted on the device before it is uploaded.
- A random per-account master key is wrapped locally in a versioned envelope
  with a password slot and a one-time offline vault-recovery slot. The hosted
  API stores the encrypted envelope and a one-way possession verifier, not the
  plaintext master key.
- When **Remember me** is enabled, the session and decrypted master key are
  stored in Windows Credential Manager. Explicit sign-out removes both; an
  expired or invalidated account session preserves the remembered key so the
  client can safely recover the existing vault after reauthentication.
- Storage access uses short-lived, account-scoped credentials issued by the
  SaveState API.
- The account password is sent to the SaveState API over HTTPS for account
  authentication. For that reason, this project describes its protection as
  **client-side encryption**, not strict zero-knowledge authentication.

See [SECURITY.md](SECURITY.md) for the supported security boundary and private
vulnerability reporting process. Network behavior is documented in
[PRIVACY.md](PRIVACY.md), and the key hierarchy and recovery invariants are
documented in [docs/VAULT_RECOVERY.md](docs/VAULT_RECOVERY.md).

## Technology

- [Tauri v2](https://v2.tauri.app/)
- Rust
- HTML, CSS, and vanilla JavaScript
- [Kopia](https://kopia.io/) for encrypted, deduplicated snapshots

## Build on Windows

Requirements:

- Windows 10 or 11 on x64
- Node.js 22
- the stable Rust toolchain
- the Windows prerequisites required by Tauri v2

```powershell
npm ci
npm run bundle:kopia
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
```

`bundle:kopia` downloads the pinned Kopia release and verifies its SHA-256
checksum before it is included in the application bundle.

`npm run build` creates reproducible unsigned local installers. Protected
release automation uses `npm run build:release`, which additionally creates
Tauri updater artifacts and therefore requires the private updater signing key
from CI secret storage.

## Code signing

The project intends to use free code signing provided by
[SignPath.io](https://about.signpath.io/), with a certificate from
[SignPath Foundation](https://signpath.org/). Until that application is
approved and the release workflow is connected, public installers may remain
unsigned. The signing policy is documented in
[CODE_SIGNING_POLICY.md](CODE_SIGNING_POLICY.md).

## Contributing

Contributions are welcome through GitHub pull requests. Start with
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

The source code and bundled project assets are licensed under
[GPL-3.0-only](LICENSE). The license does not grant permission to represent a
fork as an official SaveState product; see [TRADEMARKS.md](TRADEMARKS.md).
