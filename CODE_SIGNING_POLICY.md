# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io/),
certificate by [SignPath Foundation](https://signpath.org/).

## Roles

- Committer and reviewer: [@BareMelon](https://github.com/BareMelon)
- Signing approver: [@BareMelon](https://github.com/BareMelon)

Additional contributors submit pull requests and do not receive signing
approval rights by default. Changes from people without direct commit access
must be reviewed before merge.

## What may be signed

Only Windows application artifacts produced from this repository's reviewed
source and build scripts may be submitted for signing. Upstream open-source
components may be included in the installer but are never re-signed as if they
were authored by SaveState.

Every signing request must:

1. originate from a tagged commit in this repository;
2. use locked Node and Rust dependency manifests;
3. download the pinned Kopia artifact through `scripts/bundle-kopia.mjs` and
   pass its SHA-256 verification;
4. pass the Windows CI workflow; and
5. receive manual approval from the signing approver.

The product name and version embedded in signed artifacts must match the
release tag and package metadata.

Local and pull-request builds use `npm run build` and do not create updater
artifacts. Approved release builds use `npm run build:release`; the required
Tauri updater private key is supplied only from protected CI secret storage.

## Privacy

The client privacy policy is maintained in [PRIVACY.md](PRIVACY.md). The
program does not transfer information to networked systems except as described
there and as required by user-requested SaveState cloud operations.
