#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$root"
cargo build --locked --release -p macos-client
bundle="target/dist/Hyper Seamless X.app"
mkdir -p "$bundle/Contents/MacOS"
install -m 755 target/release/macos-client "$bundle/Contents/MacOS/hyper-seamless-x"
install -m 644 apps/macos-client/Info.plist "$bundle/Contents/Info.plist"
codesign --force --sign - "$bundle"
ditto -c -k --keepParent "$bundle" target/dist/hyper-seamless-x-macos-arm64.zip
