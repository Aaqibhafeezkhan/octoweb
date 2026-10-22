#!/usr/bin/env bash
# release.sh — Publish an Octoweb update for Sparkle (see docs/auto-updates.md):
# build and notarize, sign the DMG with the octoweb EdDSA key, upload it to the
# CDN, and write the new appcast into the website checkout.
# Usage:
#   ./release.sh               — build with ./build.sh --sign, then publish
#   ./release.sh --skip-build  — publish the notarized DMG already in dist/
#   ./release.sh --dry-run     — sign and write the appcast to a temp folder; upload nothing
# Environment:
#   OCTOWEB_R2_BUCKET    R2 bucket behind cdn.octoweb.xyz (required unless --dry-run)
#   OCTOWEB_SPARKLE_KEY  1Password reference to the private key, e.g.
#                        "op://Private/Octoweb Sparkle/password". Unset: the key is
#                        read from the Keychain (account "octoweb").
#   OCTOWEB_WEB_DIR      website checkout (default: ../octoweb-web)
#   WRANGLER             wrangler command (default: bunx wrangler)
# Release notes: this version's section of CHANGELOG.md, embedded in the appcast.
set -euo pipefail

APP_NAME="Octoweb"
CDN_BASE="https://cdn.octoweb.xyz"
SITE_URL="https://octoweb.xyz"
KEY_ACCOUNT="octoweb"
# The archive build.sh downloads; keep in step with SPARKLE_VERSION there.
SPARKLE_VERSION="2.10.0"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"  # cargo must resolve this project's manifest + config
ASSETS_DIR="$SCRIPT_DIR/assets"
WEB_DIR="${OCTOWEB_WEB_DIR:-$SCRIPT_DIR/../octoweb-web}"
APPCAST="$WEB_DIR/static/appcast.xml"
WRANGLER="${WRANGLER:-bunx wrangler}"

SKIP_BUILD=false
DRY_RUN=false
for arg in "$@"; do
  [[ "$arg" == "--skip-build" ]] && SKIP_BUILD=true
  [[ "$arg" == "--dry-run" ]] && DRY_RUN=true
done

fail() { echo "✖ $*" >&2; exit 1; }

VERSION=$(defaults read "$ASSETS_DIR/Info.plist" CFBundleShortVersionString)
IFS=. read -r V_MAJOR V_MINOR V_PATCH <<< "$VERSION"
V_MINOR=${V_MINOR:-0}
V_PATCH=${V_PATCH:-0}
DMG_NAME="$APP_NAME-$VERSION.dmg"
DMG="$SCRIPT_DIR/dist/$DMG_NAME"
DOWNLOAD_BASE="$CDN_BASE/$V_MAJOR"

# ── Preflight ─────────────────────────────────────────────────────────────────
PUBLIC_KEY=$(/usr/libexec/PlistBuddy -c "Print :SUPublicEDKey" "$ASSETS_DIR/Info.plist" 2>/dev/null || true)
[ -n "$PUBLIC_KEY" ] || fail "SUPublicEDKey is empty in assets/Info.plist: do the one-time setup in docs/auto-updates.md"
CARGO_VERSION=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
[ "$CARGO_VERSION" = "$VERSION" ] || fail "Cargo.toml says $CARGO_VERSION, Info.plist says $VERSION"
$DRY_RUN || [ -n "${OCTOWEB_R2_BUCKET:-}" ] || fail "Set OCTOWEB_R2_BUCKET to the R2 bucket behind $CDN_BASE"
[ -d "$WEB_DIR/static" ] || fail "No website checkout at $WEB_DIR (set OCTOWEB_WEB_DIR)"
if [ -f "$APPCAST" ] && grep -q "<sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>" "$APPCAST"; then
  fail "$VERSION is already in the appcast: bump the version in Cargo.toml and assets/Info.plist"
fi
# A shipped DMG is never replaced: installed copies check it against the
# signature and length in their appcast. The query string skips any cached 404.
if curl -fsSI "$DOWNLOAD_BASE/$DMG_NAME?preflight=$(date +%s)" >/dev/null 2>&1; then
  fail "$DOWNLOAD_BASE/$DMG_NAME already exists: bump the version in Cargo.toml and assets/Info.plist"
fi
if [ -n "${OCTOWEB_SPARKLE_KEY:-}" ] && ! command -v op >/dev/null; then
  fail "OCTOWEB_SPARKLE_KEY is set but the 1Password CLI (op) isn't installed"
fi

# ── Build ─────────────────────────────────────────────────────────────────────
$SKIP_BUILD || "$SCRIPT_DIR/build.sh" --sign
[ -f "$DMG" ] || fail "No $DMG: run without --skip-build"
xcrun stapler validate "$DMG" >/dev/null || fail "$DMG_NAME isn't notarized and stapled: build with ./build.sh --sign"

# Sparkle's tools come with the archive the app embeds.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
GENERATE_APPCAST="$TARGET_DIR/sparkle/$SPARKLE_VERSION/bin/generate_appcast"
[ -x "$GENERATE_APPCAST" ] || fail "generate_appcast not found: build once with ./build.sh"

# ── Sign the update and write the appcast ─────────────────────────────────────
# A temp folder holding the new DMG and the live appcast: generate_appcast adds
# the new item and keeps the two before it (its --maximum-versions default),
# whose DMGs stay on the CDN.
STAGE=$(mktemp -d)
$DRY_RUN || trap 'rm -rf "$STAGE"' EXIT
cp "$DMG" "$STAGE/"
[ -f "$APPCAST" ] && cp "$APPCAST" "$STAGE/appcast.xml"
# Release notes: this version's CHANGELOG.md section, Markdown, named after the DMG.
awk -v heading="## [$VERSION]" '
  index($0, heading) == 1 { on = 1; next }
  on && /^## \[/ { exit }
  on { print }
' CHANGELOG.md > "$STAGE/$APP_NAME-$VERSION.md"
if ! grep -q '[^[:space:]]' "$STAGE/$APP_NAME-$VERSION.md"; then
  rm "$STAGE/$APP_NAME-$VERSION.md"
  echo "⚠ No [$VERSION] section in CHANGELOG.md: the update shows no release notes"
fi

ARGS=(--download-url-prefix "$DOWNLOAD_BASE/" --link "$SITE_URL" --embed-release-notes --maximum-deltas 0)
# Every item from 1.0.0 on names its major's first build, so a copy on an older
# major is never moved across silently: Sparkle asks first. 0.x has no major.
if (( V_MAJOR > 0 )); then
  ARGS+=(--major-version "$(( V_MAJOR * 10000 ))")
fi

echo "▶ Signing $DMG_NAME and writing the appcast…"
if [ -n "${OCTOWEB_SPARKLE_KEY:-}" ]; then
  op read "$OCTOWEB_SPARKLE_KEY" | "$GENERATE_APPCAST" --ed-key-file - "${ARGS[@]}" "$STAGE"
else
  "$GENERATE_APPCAST" --account "$KEY_ACCOUNT" "${ARGS[@]}" "$STAGE"
fi
grep -q "<sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>" "$STAGE/appcast.xml" \
  || fail "generate_appcast wrote no item for $VERSION"

if $DRY_RUN; then
  echo ""
  echo "✅ Dry run: appcast and signature in $STAGE/appcast.xml. Nothing uploaded."
  exit 0
fi

# ── Upload ────────────────────────────────────────────────────────────────────
# Before the appcast: an appcast pointing at a missing DMG fails every update.
echo "▶ Uploading $DMG_NAME to $DOWNLOAD_BASE/…"
$WRANGLER r2 object put "$OCTOWEB_R2_BUCKET/$V_MAJOR/$DMG_NAME" --file "$STAGE/$DMG_NAME" \
  --content-type application/x-apple-diskimage --remote

LOCAL_SIZE=$(stat -f%z "$STAGE/$DMG_NAME")
REMOTE_SIZE=$(curl -fsSI "$DOWNLOAD_BASE/$DMG_NAME" | awk 'tolower($1) == "content-length:" { print $2 }' | tr -d '\r') \
  || fail "$DOWNLOAD_BASE/$DMG_NAME isn't reachable; the appcast was not updated"
[ "$REMOTE_SIZE" = "$LOCAL_SIZE" ] \
  || fail "The CDN serves $REMOTE_SIZE bytes for $DMG_NAME, expected $LOCAL_SIZE; the appcast was not updated"

# ── Publish the appcast ───────────────────────────────────────────────────────
cp "$STAGE/appcast.xml" "$APPCAST"

echo ""
echo "✅ Octoweb $VERSION is on the CDN; the appcast is in $APPCAST"
echo ""
echo "Next:"
echo "  1. Cloudflare Pages → octoweb → Settings → Environment variables: VERSION=$VERSION (the /download redirect)"
echo "  2. Commit static/appcast.xml in octoweb-web and push to master: the Deploy workflow publishes it"
echo "  3. Check: curl -s $SITE_URL/appcast.xml | grep $VERSION"
