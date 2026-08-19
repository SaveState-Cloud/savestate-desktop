# Contributing

Thanks for helping improve SaveState Desktop.

## Before opening a pull request

1. Create a focused branch from `main`.
2. Keep credentials, customer data, local paths, logs, and operational runbooks
   out of commits.
3. Add or update tests for behavior changes.
4. Run the same checks used by CI:

```powershell
npm ci
npm run bundle:kopia
node --check src/app.js
node --check scripts/bundle-kopia.mjs
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml
cargo check --release --manifest-path src-tauri/Cargo.toml
```

## Pull requests

Describe the user-visible impact, security implications, and validation
performed. Changes to authentication, encryption, updater behavior, storage
authorization, CI, or build scripts require especially careful review.

By contributing, you agree that your contribution is licensed under
GPL-3.0-only, the license of this repository.

## Security reports

Do not use a pull request or public issue for an undisclosed vulnerability.
Follow [SECURITY.md](SECURITY.md) instead.
