#!/usr/bin/env bash
# Installs or updates slopwatch: builds the GUI and the daemon, bundles them
# into an .app, signs it ad-hoc inside-out, quits the running GUI, swaps the
# whole bundle and relaunches it. The relaunched GUI notices the daemon runs
# the old build and replaces it: restart, unregister, register (ADR 0009).
#
#   scripts/install.sh          release build, /Applications/slopwatch.app
#   scripts/install.sh --dev    dev build, ~/Applications/slopwatch-dev.app
#
# A dev build has its own bundle ID, launchd label, data dir and socket, so
# it runs next to a release build without sharing state.
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
  "")
    flavor=release
    bundle_id=com.jnsdls.slopwatch
    name=slopwatch
    dest=/Applications/slopwatch.app
    ;;
  --dev)
    flavor=dev
    bundle_id=com.jnsdls.slopwatch.dev
    name="slopwatch dev"
    # Outside the checkout, so every worktree updates the one dev install
    # macOS registered.
    dest="$HOME/Applications/slopwatch-dev.app"
    ;;
  *)
    echo "usage: $0 [--dev]" >&2
    exit 2
    ;;
esac
# Must match Flavor::agent_label in crates/protocol.
label="$bundle_id.daemon"

echo "==> Building the $flavor flavor"
SLOPWATCH_FLAVOR=$flavor cargo build --release --locked -p slopwatch-client -p slopwatch-daemon

target_dir=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
version=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "slopwatch-client"))')

echo "==> Bundling"
staging="$target_dir/bundle/$flavor"
app="$staging/$(basename "$dest")"
rm -rf "$staging"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp "$target_dir/release/slopwatch" "$target_dir/release/slopwatchd" "$app/Contents/MacOS/"

cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>$bundle_id</string>
  <key>CFBundleName</key>
  <string>$name</string>
  <key>CFBundleDisplayName</key>
  <string>$name</string>
  <key>CFBundleExecutable</key>
  <string>slopwatch</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

# The daemon's agent. SMAppService.agent finds it by name in
# Contents/Library/LaunchAgents. KeepAlive brings the daemon back after a
# crash or a restart; it has no clean exit to honor (ADR 0009).
cat >"$app/Contents/Library/LaunchAgents/$label.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$label</string>
  <key>BundleProgram</key>
  <string>Contents/MacOS/slopwatchd</string>
  <key>AssociatedBundleIdentifiers</key>
  <array>
    <string>$bundle_id</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
PLIST

echo "==> Signing ad-hoc, inside-out"
# No --timestamp: ad-hoc signatures can't carry one. No entitlements.
codesign --force --sign - --options runtime --identifier "$label" "$app/Contents/MacOS/slopwatchd"
codesign --force --sign - --options runtime "$app"
codesign --verify --strict --deep "$app"

echo "==> Quitting the running GUI"
gui="$dest/Contents/MacOS/slopwatch"
# The pids whose executable is exactly $gui. Comparing strings keeps the
# path out of a pkill regex.
gui_pids() {
  ps -axo pid=,comm= | awk -v gui="$gui" '{ pid = $1; sub(/^ *[0-9]+ /, ""); if ($0 == gui) print pid }'
}
pids=$(gui_pids)
if [[ -n "$pids" ]]; then
  # shellcheck disable=SC2086 # one word per pid
  kill $pids 2>/dev/null || true
  for _ in $(seq 50); do
    [[ -z "$(gui_pids)" ]] && break
    sleep 0.1
  done
  pids=$(gui_pids)
  if [[ -n "$pids" ]]; then
    # shellcheck disable=SC2086 # one word per pid
    kill -9 $pids 2>/dev/null || true
  fi
fi

echo "==> Installing $dest"
mkdir -p "$(dirname "$dest")"
rm -rf "$dest.new" "$dest.old"
ditto "$app" "$dest.new"
if [[ -e "$dest" ]]; then
  mv "$dest" "$dest.old"
fi
if ! mv "$dest.new" "$dest"; then
  if [[ -e "$dest.old" ]]; then
    mv "$dest.old" "$dest"
  fi
  echo "error: couldn't move the new bundle into place, kept the old one" >&2
  exit 1
fi
rm -rf "$dest.old"

echo "==> Launching"
open "$dest"
