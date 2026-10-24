# Auto-updates (Sparkle)

Octoweb updates itself with [Sparkle 2](https://sparkle-project.org). It checks for an update once a day, downloads it in the background and installs it the next time the user quits. This document covers how that works, the one-time setup, how to publish a release, and what to check when something goes wrong.

## How it works

| Piece | Where |
|---|---|
| Updater | `src/updates.rs` (Sparkle's `SPUStandardUpdaterController` and an `SPUUpdaterDelegate`, driven through objc2) |
| Framework | Sparkle 2.10.0, the official release archive. `build.sh` downloads it once, checks its pinned SHA-256 and unpacks it into `<cargo target dir>/sparkle/2.10.0/`. Release builds embed `Sparkle.framework` in `Contents/Frameworks` |
| Settings | `assets/Info.plist`: `SUFeedURL`, `SUPublicEDKey`, `SUEnableAutomaticChecks`, `SUScheduledCheckInterval` (86400 s), `SUAutomaticallyUpdate` |
| Feed (appcast) | `https://octoweb.xyz/appcast.xml`, the file `octoweb-web/static/appcast.xml`, live when the site deploys |
| Update files | `https://cdn.octoweb.xyz/<major>/Octoweb-<version>.dmg` (R2). The website's `/download` is a Pages Function (`octoweb-web/functions/download/index.ts`) that redirects to the same file for the Pages env var `VERSION` |
| Publishing | `release.sh` |

**Loaded, not linked.** The binary does not link Sparkle. At startup `updates::start()` reads `SUFeedURL` and `SUPublicEDKey` from the bundle. When both are set it loads `Contents/Frameworks/Sparkle.framework` with `NSBundle` and looks its classes up by name. A `cargo run` binary, CI's test builds and the tar.gz archives of 0.18.1 and earlier have no framework, so they start as before with the updater off. The log says why: `updates off: …`.

**Schedule.** Sparkle stores the time of its last check and checks again once a day has passed. `SUEnableAutomaticChecks` is on in Info.plist, so Sparkle never asks the user for permission on the second launch. The updater starts with `-[SPUUpdater startUpdater:]` rather than the controller's `startUpdater`: a broken feed or key is logged, not shown to the user in an alert.

**Download and install.** Because `SUAutomaticallyUpdate` is on, Sparkle downloads the DMG in the background. It verifies the EdDSA signature and the Developer ID signature, then installs when Octoweb quits. Sparkle's installer waits for the process to end, so every quit path works. `updater:willInstallUpdateOnQuit:immediateInstallationBlock:` only logs and returns NO.

**Why never a relaunch on its own.** Session restore brings back tabs and their URLs. It does not bring back text typed into pages, private tabs, session cookies (sites that use them sign the user out), page scroll, playing media or terminal processes. So unlike a menu bar app, Octoweb never takes Sparkle's install-now handler to relaunch while the user is away. The install waits for a quit the user chose. If the user hasn't quit for a week (Sparkle's default `SUScheduledImpatientCheckInterval`), Sparkle shows its own window offering "Install and Relaunch", which the user can postpone.

**Standard UI.** Octoweb is a regular Dock app, so it uses Sparkle's standard windows. Without gentle reminders a scheduled check found while Octoweb is in the background waits until the user comes back to the app (Sparkle's `immediateFocus` behaviour). With automatic downloads on, there is no window at all until that one-week prompt.

**Where the user sees it.**
- The app menu (the "Octoweb" menu) has "Check for Updates…". It targets the controller's `checkForUpdates:`, which also enables and disables the item.
- Settings (⌘,) → General → Updates has "Check for updates daily" (`automaticallyChecksForUpdates`), "Download updates automatically" (`automaticallyDownloadsUpdates`), the running version and "Check for Updates…". Sparkle keeps both settings in Octoweb's user defaults, not `config.toml`.

**Dev builds.** `./build.sh --dev` removes `SUFeedURL` from the bundle and doesn't embed Sparkle, so the updater stays off. Settings shows "Updates are off in this build." and the app menu has no update item. While `SUPublicEDKey` is empty, release builds behave the same way, and `build.sh` warns.

**Build number.** Sparkle compares `CFBundleVersion`, not the marketing version. `build.sh` derives it from the version: `major × 10000 + minor × 100 + patch`, so 0.19.0 becomes 1900 and 1.0.0 becomes 10000. Minor and patch must stay below 100. Don't edit `CFBundleVersion` by hand. Releases before Sparkle report 1 (the DMG) or the Release workflow's run number (the tar.gz, 36 for 0.18.1), both below any new build number.

**Apple Silicon only.** The DMG `build.sh` makes on an Apple Silicon Mac holds an arm64 binary. `generate_appcast` infers the hardware requirement from the archive, so the appcast item asks for arm64 and an Intel copy would never be offered it.

**Privacy.** A check is a plain GET of `appcast.xml` from octoweb.xyz, with a user agent like `Octoweb/0.19.0 Sparkle/2.10.0`. System profiling (`SUEnableSystemProfiling`) is off. The update itself downloads from cdn.octoweb.xyz. The privacy page on octoweb.xyz and the Settings row say this.

## One-time setup

Do these steps once, on the Mac you release from. **The octoweb key already exists** (Keychain account `octoweb`, public key in `assets/Info.plist`), so skip step 2: it is only for a new app or a lost key, and a new key strands every installed copy. Steps 2 and 3 need Sparkle's tools: build once with `./build.sh --no-dmg` (or any release build) and they exist at `<target dir>/sparkle/2.10.0/bin/`. Find the target dir with `cargo metadata --format-version 1 --no-deps | jq -r .target_directory`. Below it is `$T`.

1. **Install the tools.**
   ```sh
   brew install 1password-cli
   bunx wrangler login        # Cloudflare account that owns the R2 bucket behind cdn.octoweb.xyz
   ```
   In the 1Password app, turn on Settings → Developer → "Integrate with 1Password CLI".

2. **Generate the signing key.** It goes under the Keychain account `octoweb`, which keeps it separate from any other Muvon app:
   ```sh
   $T/sparkle/2.10.0/bin/generate_keys --account octoweb
   ```
   The command prints the public key. Paste it into `assets/Info.plist` as the `SUPublicEDKey` string and commit it. The public key is not a secret.

3. **Move the private key to 1Password.**
   ```sh
   $T/sparkle/2.10.0/bin/generate_keys --account octoweb -x octoweb-sparkle.key
   ```
   In 1Password, create a **Password** item named **Octoweb Sparkle** (not SSH Key: that type only takes OpenSSH keys) and paste the file's contents into its **password** field. Then delete the file: `rm octoweb-sparkle.key`.

   This key is the only thing that can sign Octoweb updates. If it is lost, installed copies can never update again. If it leaks, someone who also controls the feed could ship a fake update.

4. **Set the environment** in your shell profile:
   ```sh
   export OCTOWEB_SPARKLE_KEY="op://Private/Octoweb Sparkle/password"   # 1Password: field menu → Copy Secret Reference
   export OCTOWEB_R2_BUCKET="<bucket behind cdn.octoweb.xyz>"
   export CLOUDFLARE_ACCOUNT_ID="<account id>"   # only if your Cloudflare login sees more than one account
   export OCTOWEB_WEB_DIR="$HOME/Work/dev/muvon/octoweb-web"   # only if it isn't ../octoweb-web
   ```
   The bucket must have `cdn.octoweb.xyz` connected as its custom domain, with status Active (R2 → the bucket → Settings → Custom Domains). `release.sh` checks every upload through that domain and stops before touching the appcast if it can't reach the DMG.

5. **Check that signing works from 1Password.** This needs a notarized DMG built after step 2, so it carries the public key:
   ```sh
   ./build.sh --sign
   ./release.sh --skip-build --dry-run
   ```
   It should print "Dry run: appcast and signature in …". `generate_appcast` refuses to sign if the key in 1Password doesn't match `SUPublicEDKey` in the app.

6. **Remove the Keychain copy**, so the key lives only in 1Password:
   ```sh
   security delete-generic-password -a octoweb -s "https://sparkle-project.org"
   ```
   If the command finds nothing, search Keychain Access for "Sparkle" and delete the item for account `octoweb`. Without `OCTOWEB_SPARKLE_KEY`, `release.sh` falls back to the Keychain, so this step also makes sure every release signs from 1Password.

**Releases before Sparkle.** Copies of 0.18.1 and earlier have no updater. Their users install the first release with Sparkle by hand, from the website. Every later release then updates on its own.

## Publishing a release

1. Bump the version in `Cargo.toml` and `assets/Info.plist` (they must match), write its `CHANGELOG.md` section (Sparkle shows that section, as Markdown, in the update window), commit, and push the tag (`0.19.0`, no `v`). CI (`.github/workflows/release.yml`) checks the tag against `Cargo.toml`, builds with `./build.sh --sign` (build, embed and sign Sparkle, sign, DMG, notarize, staple) and attaches the DMG to a **draft** GitHub release.
2. Fetch CI's DMG and publish it: `git pull`, `gh release download <version> --repo Muvon/octoweb --pattern '*.dmg' --dir dist`, then `./release.sh --skip-build`. (Without CI: plain `./release.sh` builds locally first.) The script:
   - checks the public key, that `Cargo.toml` and Info.plist agree, the bucket and the web checkout, and refuses a version already in the appcast or already on the CDN (a shipped DMG is never replaced);
   - without `--skip-build`, runs `./build.sh --sign`; either way it checks the staple;
   - copies the DMG, the live `octoweb-web/static/appcast.xml` and this version's `CHANGELOG.md` section to a temp folder;
   - runs `generate_appcast`. It signs the DMG with the key from 1Password, adds the new item and keeps the two before it (`generate_appcast` keeps three versions by default). From 1.0.0 on, every item gets `--major-version` (the major's first build number, `major × 10000`), so Sparkle asks before moving a copy from an older major. 0.x releases pass none;
   - uploads the DMG to `<bucket>/<major>/Octoweb-<version>.dmg`, then checks that the CDN serves the same number of bytes;
   - only then writes the new appcast to `octoweb-web/static/appcast.xml`.

   `generate_appcast` comes from the Sparkle archive `build.sh` unpacks, so the release Mac needs one local release build (`./build.sh --no-dmg`) before the first `--skip-build`.
3. In Cloudflare Pages → octoweb → Settings → Environment variables, set `VERSION` to the new version. The `/download` redirect uses it.
4. Commit `static/appcast.xml` in octoweb-web and push to master. The Deploy workflow publishes it, and the update goes live with the site.
5. Check that it's live: `curl -s https://octoweb.xyz/appcast.xml | grep <version>`. In an installed older version, use Check for Updates….
6. Publish the draft GitHub release. That fires `notify-tap.yml`, and the tap's `update-octoweb.yml` bumps the cask from the CDN DMG — the same file Sparkle signed. No GitHub release? `gh workflow run notify-tap.yml --repo Muvon/octoweb -f version=<version>`.

Never upload any DMG by hand: only `release.sh` uploads, and only the file it just signed.

The DMG must never be re-uploaded after signing. The appcast carries its signature and length, and a different file fails verification on every Mac.

## When a release goes wrong

- **Stop it spreading.** Revert `static/appcast.xml` in octoweb-web and deploy. Macs that haven't downloaded the update yet won't see it. Sparkle never downgrades, so Macs that already installed it keep it.
- **Fix forward.** Bump the patch version and release again.
- **A run stopped after the upload** (the DMG is on the CDN, the appcast has no item for it). Run `./release.sh --skip-build` again with the same DMG in `dist/`. It finds the identical file on the CDN, skips the upload and writes the appcast. A different file at that URL still stops it.

## Troubleshooting

- **Octoweb's update log:** run the app from Terminal, `RUST_LOG=octoweb::updates=debug /Applications/Octoweb.app/Contents/MacOS/octoweb`. Look for `updater started`, `updates off: …`, `update downloaded; installs when Octoweb quits` and `update aborted`.
- **Sparkle's own log:** `/usr/bin/log show --predicate 'subsystem == "org.sparkle-project.Sparkle"' --info --last 1d`
- **"Updates are off in this build":** a `--dev` build, a tar.gz from 0.18.1 or earlier (no framework), or `SUPublicEDKey` was empty when the app was built.
- **"Sparkle.framework is not in the app bundle":** the bundle wasn't made by `build.sh` without `--dev`.
- **Signature error on the client:** the DMG on the CDN isn't the one that was signed, or the app was built with a different public key.
- **The download in `build.sh` fails its checksum:** the archive on GitHub isn't the pinned one. Don't update the hash without checking why; a version bump changes both `SPARKLE_VERSION` and `SPARKLE_SHA256` in `build.sh`, and `SPARKLE_VERSION` in `release.sh`.

## Homebrew cask

The cask (`muvon/homebrew-tap`, `Casks/octoweb.rb`) installs the DMG `release.sh` uploads, `https://cdn.octoweb.xyz/#{version.major}/Octoweb-#{version}.dmg`, with `depends_on arch: :arm64`. Publishing the GitHub release fires `notify-tap.yml`, and the tap's `update-octoweb.yml` downloads that DMG, computes its `sha256` and bumps the cask. The cask declares `auto_updates true`, so `brew upgrade` leaves alone a copy Sparkle has already updated.

The flip side: `brew upgrade` skips auto-updating casks, so a brew install of 0.18.1 or earlier (the old tar.gz, no Sparkle) is never moved to the DMG on its own. Those users run `brew upgrade --cask --greedy octoweb` once; every release after that updates through Sparkle.

## CI

`.github/workflows/release.yml` runs on a tag push, on `macos-15` (arm64): it checks the tag against `Cargo.toml`, sets `CFBundleShortVersionString` from the tag, runs `./build.sh --sign` and attaches `Octoweb-<version>.dmg` and its `.sha256` to a draft release. It needs these repository secrets:

| Secret | For |
|---|---|
| `DEVELOPER_ID_P12`, `DEVELOPER_ID_PASSWORD` | the Developer ID Application certificate (base64 .p12) and its password |
| `APPLE_ID`, `APPLE_APP_PASSWORD` | `notarytool`, stored as the `Muvon-Notarize` profile `build.sh` uses |
| `OCTOWEB_GOOGLE_CLIENT_ID`, `OCTOWEB_GOOGLE_CLIENT_SECRET` | optional: the Google sign-in client for Gmail, compiled in like a local build's Keychain items |
| `TAP_GITHUB_TOKEN` | `notify-tap.yml`: the repository dispatch to `muvon/homebrew-tap` |

Sparkle publishing stays local, in `release.sh`, because it needs the EdDSA key from 1Password, the R2 upload and a commit to octoweb-web.
