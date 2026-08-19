# Third-party software

SaveState Desktop depends on open-source software whose licenses remain in
effect.

## Kopia

The build downloads the official Kopia executable from
<https://github.com/kopia/kopia>. Kopia is licensed under Apache-2.0. The exact
version and expected archive checksum are defined in
`scripts/bundle-kopia.mjs`.

## Tauri, Rust crates, and Node packages

Tauri, Rust dependencies, and Node dependencies are declared in
`src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, `package.json`, and
`package-lock.json`. Their own copyright notices and licenses apply. Locked
dependency manifests are retained to support review and reproducible builds.
