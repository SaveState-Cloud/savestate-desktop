# SaveState Windows app channels

Development, staging, and production are separately installable Windows applications with distinct bundle identifiers.

- Development talks only to api-dev.savestate.dk and uses the development updater.
- Staging talks only to api-staging.savestate.dk and uses the staging updater.
- Production talks only to api.savestate.dk and uses the stable updater.

Development and staging builds are internal GitHub Actions artifacts with a 14-day retention. They never create a public GitHub release, stable tag, or production updater manifest.

Run npm run test:js, npm run test:ui, cargo test, and the matching Tauri build before distributing an internal build.
