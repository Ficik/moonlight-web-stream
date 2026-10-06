# Moonlight Web Desktop

The `desktop` branch adds a reliable WebRTC clipboard data channel and reliable,
ordered keyboard/button delivery for desktop use. It requires the paired HTTPS
clipboard extension in [Ficik/Sunshine](https://github.com/Ficik/Sunshine/tree/desktop).
Clipboard text is transferred as data; paste sends one Ctrl+V or Ctrl+Shift+V
shortcut after the host confirms it owns the clipboard. Text is limited to 1 MiB.
Use HTTPS and allow browser clipboard permissions. Remote copy requires the
stream to have focus; the sidebar provides a user-gesture clipboard-copy fallback.

Upstream base: `e491a3b21e49d4b6b4b0a6e97bc10bb8c0904b05`.
Cargo.toml/Cargo.lock and ubrn.config.yaml pin the same exact commit from
Ficik/moonlight-common-rust. No sibling checkout is required for release builds.

## Build and run

Use Rust 1.99.0 with rustfmt and the wasm32-unknown-unknown target, Node 26.4.0,
npm 11.17.0, and wasm-bindgen-cli 0.2.128. CI provides the complete build recipe.

```sh
npm ci
npm run build
cargo build --release --locked
./target/release/web-server --config-path /path/to/config.json
```

Run from the repository root, or from the extracted release directory containing
`web-server` and `dist/`. Use a persistent directory for configuration, users,
TLS keys and host pairing state. Never store those files in this repository or
replace them when extracting a new release. Existing upstream server configuration
options and host pairing flows are unchanged.

For local library development, add an untracked `.cargo/config.toml`:

```toml
[patch."https://github.com/Ficik/moonlight-common-rust.git"]
moonlight-common = { path = "../moonlight-common-rust" }
```

Cargo resolves this path relative to the directory above `.cargo`; adjust it to
your checkout layout (for adjacent repositories use `../moonlight-common-rust`).
This local override changes Cargo.lock; restore the committed Git-pinned lockfile
before committing/releasing. For binding changes, update the pinned library
revision and regenerate with `npm run prebuild`.

## CI and upstream upgrades

Pushes to `desktop` and `upgrade/**` run Rust/clipboard tests, regenerate and check
TypeScript/WASM, and build a Linux amd64 gateway archive. Tags `desktop-v*` publish
that archive to GitHub Releases after CI passes, using GITHUB_TOKEN. The archive
is built on Ubuntu 22.04, for deployment on Ubuntu 22.04/24.04 amd64.

Keep origin pointed to your fork and upstream to MrCreativ3001/moonlight-web-stream.
Rebase an `upgrade/<version>` branch onto the chosen upstream revision with
`git rebase --onto <new-base> <recorded-old-base>`. Keep clipboard/input and CI
changes separate. Rebase/update the common Rust library first, then pin its
tested commit in both Cargo.toml and ubrn.config.yaml and update Cargo.lock
without upgrading unrelated dependencies. CI and desktop smoke tests must pass
before updating desktop and publishing a new tag. Never rewrite released tags.

Reload all browser stream tabs after deploying updated web assets, so they use
the new input implementation. X11 clipboard integration tests must run on an idle,
isolated desktop; other viewers can change focus and invalidate test results.
