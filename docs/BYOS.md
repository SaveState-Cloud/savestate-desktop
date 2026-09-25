# Customer-owned storage (BYOS)

Implementation note for the next desktop release. This is **not** a claim that
the currently installed production app already has BYOS.

## Customer flow

1. On an active Pro or Ultra plan, open **Settings → Your storage destinations**.
2. Create a bucket with an S3-compatible provider and enter its endpoint,
   region, bucket name, access key ID and secret. A dedicated bucket or prefix
   is recommended. SaveState generates an account-scoped prefix if left blank.
3. **Connect and test** opens or creates a Kopia repository and runs Kopia's
   storage-provider validation before the destination is saved.
4. Create a file/folder backup profile and choose the connected destination.
   SaveState-managed storage remains the default. A profile's destination is
   fixed after creation; create a second profile to change it without moving or
   deleting existing restore points.
5. Run or schedule the profile normally. Open **Restore points** in Settings to
   browse snapshots directly from the bucket and restore into a new local
   folder. Disconnecting a destination never deletes remote objects.

## Boundaries and recovery

- Customer-owned object bytes do not count toward SaveState's managed-storage
  quota, and provider charges are paid by the customer. The existing automated
  schedule limit still applies to scheduled BYOS profiles.
- File/folder profiles are supported. Quick backups and native database
  profiles still use SaveState-managed storage.
- The S3 key pair stays in Windows Credential Manager on the device. The API
  stores only plan entitlement; it never receives these keys or object bytes.
  Destination metadata and profile mappings are local SQLite data.
- Kopia encrypts the repository with the account's client-side master key.
  The same account key plus the provider credentials, endpoint, bucket and
  prefix are needed to reconnect on a replacement PC. SaveState cannot recover
  lost provider credentials. Record the displayed prefix before replacing the
  device.
- Existing customer-owned restore points remain browsable after the plan
  lapses, provided the user can sign in and unlock the same master key. New
  BYOS destinations and backups require an active eligible plan.
- The website vault cannot directly browse customer-owned objects because
  SaveState does not hold the customer's storage credentials. Use the desktop
  app's Restore points view for BYOS.
- The initial Windows release accepts HTTPS S3-compatible endpoints, plus
  loopback HTTP for local MinIO testing. Arbitrary remote HTTP is rejected.

## Verification before release

- API unit/runtime tests and desktop Rust/UI tests must pass.
- Disposable local S3 test: provider validation, connect/create, tagged
  snapshot, list, restore and file-content comparison. The 2026-09-24 local
  emulator round trip passed; it did **not** exercise a live Backblaze B2 or
  Cloudflare R2 account, app login, scheduled execution, or Windows installer.
- Before production, repeat connect/backup/restore with a disposable provider
  bucket and a signed development app, verify cancellation/sign-out and a
  replacement-PC reconnect, then release the matching API before the desktop
  installer. The public Terms and Privacy notice currently describe only
  SaveState-managed Backblaze storage and automatic deletion at expiry; update
  them to distinguish customer-owned objects, provider region/charges, and
  their separate retention before a BYOS production launch. Do not advertise
  BYOS on the live website until these gates pass.
