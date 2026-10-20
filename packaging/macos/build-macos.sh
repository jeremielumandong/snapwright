#!/usr/bin/env bash
# Build Snapwright.app and package it as a .dmg and a .pkg.
#
# Usage: packaging/macos/build-macos.sh [--arch universal|arm64|x86_64] [--no-dmg] [--no-pkg] [--skip-build]
#
# Optional signing / notarization (skipped when unset):
#   MACOS_SIGN_IDENTITY       "Developer ID Application: Name (TEAMID)"  -> codesign app + dmg
#   MACOS_INSTALLER_IDENTITY  "Developer ID Installer: Name (TEAMID)"    -> sign .pkg
#   MACOS_NOTARY_PROFILE      keychain profile from `xcrun notarytool store-credentials`
#   MACOS_NOTARY_KEYCHAIN     keychain holding that profile (default: the login keychain; CI uses a temporary one)
# shellcheck source=packaging/lib.sh
source "$(dirname "$0")/../lib.sh"

ARCH="universal"
MAKE_DMG=1
MAKE_PKG=1
SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --no-dmg) MAKE_DMG=0; shift ;;
    --no-pkg) MAKE_PKG=0; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || die "macOS packaging must run on macOS"
need cargo "Install Rust from https://rustup.rs"
need lipo "Install the Xcode command line tools: xcode-select --install"

# ScreenCaptureKit's SCScreenshotManager, the only capture path, needs macOS 14; recording
# sound links Core Audio's process taps (AudioHardwareCreateProcessTap), which are 14.2+.
MIN_MACOS="${MIN_MACOS:-14.2}"
export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

case "$ARCH" in
  universal) TARGETS=(aarch64-apple-darwin x86_64-apple-darwin) ;;
  arm64)     TARGETS=(aarch64-apple-darwin) ;;
  x86_64)    TARGETS=(x86_64-apple-darwin) ;;
  *) die "--arch must be universal, arm64 or x86_64" ;;
esac

cd "$REPO_ROOT" || exit 1

if [[ $SKIP_BUILD -eq 0 ]]; then
  for t in "${TARGETS[@]}"; do
    rustup target add "$t" >/dev/null
    log "cargo build --release --target $t"
    cargo build --release --locked --target "$t" -p snapwright-app -p snapwright-cli
  done
fi

WORK="$REPO_ROOT/target/package/macos"
rm -rf "$WORK" && mkdir -p "$WORK" "$DIST_DIR"

# --- binaries (lipo into universal when building both) -------------------------------------
merge_bin() {
  local name="$1" out="$2" inputs=()
  for t in "${TARGETS[@]}"; do
    local p="target/$t/release/$name"
    [[ -f "$p" ]] || die "missing $p (run without --skip-build)"
    inputs+=("$p")
  done
  if [[ ${#inputs[@]} -gt 1 ]]; then
    lipo -create "${inputs[@]}" -output "$out"
  else
    cp "${inputs[0]}" "$out"
  fi
  chmod +x "$out"
}

# --- app bundle ----------------------------------------------------------------------------
APP="$WORK/$APP_NAME.app"
log "assembling $APP_NAME.app ($ARCH)"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
merge_bin "$APP_BIN" "$APP/Contents/MacOS/$APP_BIN"
merge_bin "$CLI_BIN" "$APP/Contents/MacOS/$CLI_BIN"

sed -e "s|@APP_NAME@|$APP_NAME|g" -e "s|@APP_BIN@|$APP_BIN|g" -e "s|@APP_ID@|$APP_ID|g" \
    -e "s|@VERSION@|$VERSION|g" -e "s|@MIN_MACOS@|$MIN_MACOS|g" \
    "$PACKAGING_DIR/macos/Info.plist.in" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp "$REPO_ROOT/crates/editor/assets/Geist-OFL.txt" "$APP/Contents/Resources/"

# Rendered from packaging/icons/snapwright.svg by packaging/icons/generate.py (Apple's icon grid).
cp "$PACKAGING_DIR/icons/snapwright.icns" "$APP/Contents/Resources/$APP_NAME.icns"

# Extended attributes (quarantine, provenance) break signatures and leak into the .pkg as ._ files.
xattr -cr "$APP"
export COPYFILE_DISABLE=1

# --- signing -------------------------------------------------------------------------------
if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  log "codesigning with '$MACOS_SIGN_IDENTITY'"
  ENT="$PACKAGING_DIR/macos/entitlements.plist"
  codesign --force --timestamp --options runtime --entitlements "$ENT" \
    --sign "$MACOS_SIGN_IDENTITY" "$APP/Contents/MacOS/$CLI_BIN"
  codesign --force --timestamp --options runtime --entitlements "$ENT" \
    --sign "$MACOS_SIGN_IDENTITY" "$APP"
  codesign --verify --strict --verbose=2 "$APP"
else
  log "MACOS_SIGN_IDENTITY not set: ad-hoc signing (local use only)"
  codesign --force --deep --sign - "$APP"
fi

notarize() {
  [[ -n "${MACOS_NOTARY_PROFILE:-}" ]] || return 0
  log "notarizing $(basename "$1")"
  local kc=()
  [[ -n "${MACOS_NOTARY_KEYCHAIN:-}" ]] && kc=(--keychain "$MACOS_NOTARY_KEYCHAIN")
  xcrun notarytool submit "$1" --keychain-profile "$MACOS_NOTARY_PROFILE" ${kc[@]+"${kc[@]}"} --wait
  # Stapling downloads the ticket from Apple's CloudKit, which sometimes drops the connection.
  local attempt
  for attempt in 1 2 3 4 5; do
    xcrun stapler staple "$1" && return 0
    [[ $attempt -lt 5 ]] || die "stapler staple failed $attempt times"
    log "stapler staple failed (attempt $attempt); retrying in $((attempt * 15)) s"
    sleep $((attempt * 15))
  done
}

BASENAME="$APP_NAME-$VERSION-macos-$ARCH"

# --- dmg -----------------------------------------------------------------------------------
if [[ $MAKE_DMG -eq 1 ]]; then
  DMG="$DIST_DIR/$BASENAME.dmg"
  log "creating $(basename "$DMG")"
  STAGE="$WORK/dmg"
  mkdir -p "$STAGE"
  ditto --norsrc --noextattr "$APP" "$STAGE/$APP_NAME.app"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
  if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$DMG"
  fi
  notarize "$DMG"
fi

# --- pkg -----------------------------------------------------------------------------------
if [[ $MAKE_PKG -eq 1 ]]; then
  PKG="$DIST_DIR/$BASENAME.pkg"
  log "creating $(basename "$PKG")"
  ROOT="$WORK/pkgroot"
  mkdir -p "$ROOT/Applications"
  ditto --norsrc --noextattr "$APP" "$ROOT/Applications/$APP_NAME.app"
  # Drop quarantine and similar attributes. com.apple.provenance cannot be removed; pkgbuild stores
  # it as ._ entries that the installer restores as attributes, so the signature still verifies.
  xattr -cr "$ROOT" 2>/dev/null || true
  # Install exactly to /Applications, even if another copy of the bundle exists elsewhere.
  pkgbuild --analyze --root "$ROOT" "$WORK/component.plist" >/dev/null
  plutil -replace 0.BundleIsRelocatable -bool NO "$WORK/component.plist"
  sign_args=()
  [[ -n "${MACOS_INSTALLER_IDENTITY:-}" ]] && sign_args=(--sign "$MACOS_INSTALLER_IDENTITY" --timestamp)
  rm -f "$PKG"
  pkgbuild --root "$ROOT" --component-plist "$WORK/component.plist" \
    --identifier "$APP_ID" --version "$VERSION" --install-location / \
    --scripts "$PACKAGING_DIR/macos/pkg-scripts" ${sign_args[@]+"${sign_args[@]}"} "$PKG" >/dev/null
  notarize "$PKG"
fi

log "done -> $DIST_DIR"
ls -lh "$DIST_DIR/$BASENAME".* 2>/dev/null || true
