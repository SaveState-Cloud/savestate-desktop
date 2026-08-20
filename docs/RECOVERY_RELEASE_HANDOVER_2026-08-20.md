# Account recovery and public desktop rollout handover

This document is the deployment and continuation record for the coordinated
account-recovery, vault-recovery, logout, dependency, and public desktop
release work prepared on 2026-08-20. It contains no credentials. Secret names
below are configuration requirements, not values.

The cryptographic format and client invariants are specified in
[`VAULT_RECOVERY.md`](VAULT_RECOVERY.md). The service-facing privacy boundary
is specified in [`../PRIVACY.md`](../PRIVACY.md), and interrupted release
recovery is specified in [`../SECURITY.md`](../SECURITY.md).

## Account recovery is not vault recovery

These operations must remain visibly and technically separate:

- **Account recovery** proves access to the verified email address, an enrolled
  TOTP authenticator, or a one-time account recovery code. It may replace the
  login password and increments `users.auth_version`, invalidating every older
  API, browser, desktop, repository, and Engine replay session.
- **Vault recovery** proves possession of the client-owned account master key
  (AMK). It requires the previous vault password, the one-time offline vault
  recovery key, or a matching remembered-device AMK. Email, TOTP, an account
  recovery code, SaveState support, and a bearer token cannot decrypt existing
  backup ciphertext.

A password reset must never replace `encrypted_master_key`, synthesize a new
AMK for an existing vault, or describe account access as restored backup
access. A recovered login can therefore be valid while the vault remains
locked.

## Version 1 envelope contract

- The desktop generates a random 32-byte AMK. The AMK and repository password
  never reach the API.
- `keyId` is the first 18 bytes of
  `SHA-256("savestate-envelope-key-id-v1" || AMK)`, encoded as unpadded
  base64url. A remembered key is accepted only when its derived identifier
  matches.
- A version 1 envelope contains exactly one `password` slot and one
  `offline_recovery` slot. Unknown metadata and supported future slots are
  preserved during password-slot rotation.
- Each slot wraps the same AMK with AES-256-GCM and a random 96-bit nonce. The
  authenticated associated data binds envelope version, `keyId`, slot ID,
  slot type, KDF identifier, KDF parameters, and salt.
- The password slot uses Argon2id 0x13 with 19,456 KiB memory, two iterations,
  parallelism one, and a 32-byte output. These serialized parameters are part
  of the authenticated format; library defaults are not the format.
- The offline recovery value is 32 random bytes represented as unpadded
  base64url. Its wrapping key is
  `HMAC-SHA-256(recoveryKey, "savestate-offline-vault-slot-v1")`. The UI shows
  it once and does not commit the initial envelope until the user acknowledges
  saving it.
- The rotation proof is
  `HMAC-SHA-256(AMK, "savestate-envelope-verifier-v1")`, encoded as unpadded
  base64url. The API stores only `SHA-256(proof)`, never the reusable proof or
  AMK. Rotation requires that proof and an optimistic revision that advances
  by exactly one.
- Legacy ciphertext is retained during migration. A legacy vault reset before
  envelope migration still requires its previous vault password; a reset
  bearer token cannot establish a new authoritative envelope for it.

## Logout and trusted-device behavior

Explicit user sign-out and server-side session invalidation intentionally have
different local effects:

- **Explicit sign-out** first prepares a logout barrier. If backups are active,
  the UI names them and asks for confirmation. Cancelling the dialog aborts the
  barrier and leaves the authenticated session and backups intact. Confirming
  stops the active operations, cleans up their uncommitted work, invalidates
  the desktop session generation, clears in-memory authentication and
  repository caches, and deletes the remembered session and AMK from Windows
  Credential Manager.
- **`auth_version`/401 invalidation** clears the in-memory token, account email,
  AMK, pending unlock state, and repository-session cache, so scheduled work
  cannot continue under a revoked session. It deliberately does **not** delete
  the Credential Manager AMK. That remembered key may be the user's only
  remaining factor for proving and rotating the existing vault after account
  recovery.

Do not route an authorization failure through the explicit logout command.
Do not add a server-side AMK escrow as a shortcut. A future trusted-device
management UI should be an explicit opt-in/revoke feature built on the same
key-slot boundary.

## Coordinated pull requests and branches

The rollout is a set; review it as one security boundary even though the
repositories deploy independently:

- API: [SaveState-Cloud/savestate-api PR #19](https://github.com/SaveState-Cloud/savestate-api/pull/19),
  branch `codex/account-recovery-api`, prepared commit `9185d71`. This owns D1
  migration `0008_account_recovery.sql`, recovery factors, `auth_version`, the
  envelope/verifier endpoints, and the account-recovery service contract.
- Website: [SaveState-Cloud/savestate-website PR #11](https://github.com/SaveState-Cloud/savestate-website/pull/11),
  branch `codex/account-recovery-website`, prepared commits `a12afc3` and
  `dbbb209`. This owns account-recovery/TOTP UI, fragment-token handling, and
  matching privacy copy.
- Private migration source: [SaveState-Cloud/savestate-app PR #16](https://github.com/SaveState-Cloud/savestate-app/pull/16),
  branch `codex/security-recovery-integration`, validated commit `df36332`.
  The older startup-only [PR #14](https://github.com/SaveState-Cloud/savestate-app/pull/14)
  is superseded by #16; #16 preserves its one-time default and hardening.
- Public authoritative client: branch `codex/public-authoritative-parity` in
  `SaveState-Cloud/savestate-desktop`. Its runtime sources, recovery tests,
  logout tests, capability source, and Cargo lock are copied from validated
  private commit `df36332`, while GPL, public contribution/security/privacy
  documents, public release configuration, and repository metadata remain
  public-specific.
- Public dependency PRs
  [#1](https://github.com/SaveState-Cloud/savestate-desktop/pull/1),
  [#2](https://github.com/SaveState-Cloud/savestate-desktop/pull/2),
  [#3](https://github.com/SaveState-Cloud/savestate-desktop/pull/3),
  [#4](https://github.com/SaveState-Cloud/savestate-desktop/pull/4),
  [#5](https://github.com/SaveState-Cloud/savestate-desktop/pull/5),
  [#6](https://github.com/SaveState-Cloud/savestate-desktop/pull/6), and
  [#7](https://github.com/SaveState-Cloud/savestate-desktop/pull/7) are
  integrated together in the authoritative branch. PRs #1 (`rusqlite`) and #2
  (`aes-gcm`) require source migrations, and recovery plus `sha2` 0.11 requires
  `hmac` 0.13. Do not merge the independent lockfile PRs one by one after the
  integration branch; close them as superseded after the combined branch
  passes required checks and merges.

The public repository becomes the release source after this cutover. Do not
create a post-cutover Windows release from the private repository.

## Production prerequisites

Before deploying API PR #19:

1. Take a restorable D1 backup and record its identifier outside this public
   repository.
2. Review and apply `migrations/0008_account_recovery.sql` to the production
   `savestate-db`. Confirm the migration appears in the remote migration list.
3. Create an independent, high-entropy secret of at least 32 characters using
   `wrangler secret put MFA_ENCRYPTION_KEY`. Do not reuse `JWT_SECRET`, a B2
   key, a Stripe key, or the AMK.
4. Configure and verify the Cloudflare Email Sending binding named `EMAIL` for
   the API Worker. Verify `noreply@savestate.dk` is an allowed sender and that
   recovery mail reaches a real test mailbox. `wrangler deploy --dry-run` does
   not prove that a dashboard-managed production email binding exists.
5. Confirm `/health` reports the expanded recovery schema before enabling the
   website UI.

Before tagging a public desktop release, configure the GitHub release
environment without placing values in the repository:

- secrets: `TAURI_SIGNING_PRIVATE_KEY`, optional
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, `R2_ACCESS_KEY_ID`, and
  `R2_SECRET_ACCESS_KEY`;
- variables: `R2_ENDPOINT_URL`, `R2_PUBLIC_BASE_URL`, and
  `R2_RELEASE_BUCKET`.

Use least-privilege R2 credentials scoped to the application release bucket.
The hosted API's B2 provider credentials never belong in the client or public
repository.

## Release order

1. Merge API PR #19 after review, D1 backup, migration, secret, and email-binding
   preparation. Deploy the API and exercise email, TOTP, recovery-code,
   session-revocation, envelope-initialize, and envelope-rotate paths against a
   disposable production test account.
2. Merge and deploy website PR #11. Verify that a recovery bearer grant is read
   only from the exact URL fragment and that the fragment is immediately
   removed from browser history/state.
3. Merge the public authoritative desktop branch after Windows CI, RustSec, and
   the matrix below pass. Mark private PR #16/#14 and public dependency PRs
   #1-#7 as superseded only after their replacement commit is on the target
   branch.
4. Bump every desktop version field to the same next patch version. The first
   authoritative public release should be `v2.0.15`; never overwrite or relabel
   existing `v2.0.14` assets.
5. Create the stable `v2.0.15` tag on a commit contained in public `main`. Let
   `.github/workflows/release.yml` build once and publish the exact same staged
   MSI/EXE bytes, updater signature, hashes, and provenance to the GitHub
   Release and immutable R2 version prefix. Verify read-back hashes and both
   updater/download manifests before announcing the release.

The release workflow accepts only exact `vMAJOR.MINOR.PATCH` tags; prerelease or
malformed tags do not participate in newest-stable selection.

## Rollback and interrupted release policy

- The D1 migration is additive. A normal Worker rollback leaves the new tables
  and columns in place; do not run an ad-hoc destructive down migration. If the
  migration itself is corrupt, stop recovery traffic and restore the recorded
  D1 backup under an explicit incident plan.
- API or website regressions can roll back to the previous Worker/Pages
  deployment while the additive schema remains. Keep the recovery UI disabled
  until the API health/schema and email binding are verified again.
- Versioned R2 release paths and published artifact bytes are immutable. Never
  overwrite `v2.0.14`, a partially published new version prefix, or a mismatched
  GitHub Release. Follow the provenance/hash recovery procedure in
  `SECURITY.md`; if provenance cannot be proven, abandon that version and use a
  new patch version.
- If a bad newest manifest is exposed, restore the shared latest manifests to
  the last verified stable version to stop additional upgrades. This does not
  downgrade machines that already updated. Fix those machines with a higher,
  correctly signed patch release; do not republish different bytes under the
  same tag.

## Required validation matrix

The prepared set was validated with the following commands and expected
results. Rerun the full matrix after conflict resolution or version changes.

### API PR #19

```powershell
pnpm test
pnpm exec wrangler deploy --dry-run
```

Expected: 93/93 Node tests pass and Wrangler successfully assembles the Worker.
Two tests intentionally log simulated email-provider and old-object-deletion
failures while asserting fail-safe behavior; those injected log messages are
not test failures.

### Website PR #11

```powershell
pnpm test
pnpm run typecheck
pnpm run build
```

Expected: 2/2 fragment-token tests pass, TypeScript reports no error, and Vite
produces the production bundle.

### Public desktop authoritative branch

```powershell
npm ci
npm audit --audit-level=high
npm run test:js
npm run test:ui
node --check src/app.js
node --check src/logout-flow.js
node --check src/vault-recovery.js
node --check scripts/bundle-kopia.mjs
npm run bundle:kopia
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml
cargo check --release --manifest-path src-tauri/Cargo.toml
```

Expected: npm audit reports zero vulnerabilities; logout tests pass 3/3;
recovery UI tests pass 5/5; Kopia 0.23.1 downloads and passes its pinned SHA-256
check; Rust tests pass 50/50; formatting and optimized Windows checks are clean.
Also lint `.github/workflows/release.yml`, run a tracked-file secret scan, and
confirm that the functional paths still match private integration commit
`df36332` exactly. Generated Tauri schema changes created by local checks are
build output and must not be committed.

## GPL and Windows SmartScreen

Publishing the desktop source under GPL-3.0-only makes the shipped client
auditable and satisfies the public-source requirement for the intended
SignPath Foundation application. It does **not** by itself remove Microsoft
Defender SmartScreen warnings. SmartScreen also depends on Windows
Authenticode signing and reputation. Until the SignPath workflow and
certificate are approved and connected, installers may remain Authenticode
unsigned and Windows may warn. Tauri's updater signature protects update
integrity but is not an Authenticode signature and does not establish
SmartScreen reputation.
