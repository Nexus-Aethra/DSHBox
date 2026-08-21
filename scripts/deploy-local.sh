#!/bin/bash
# Build release binaries and drop them into the installed DSH Box layout,
# bypassing dpkg (sandbox has no sudo). Mirrors the deb's file placement:
#   /usr/lib/dshbox/server/linux-x64/dshboxd   <- sidecar daemon
#   /usr/lib/dshbox/runtime/linux-x64/         <- bundled Node/pnpm runtime
#   /usr/bin/dshbox                            <- desktop/CLI binary
# Requires: user in sudo group with NOPASSWD, or run the cp commands manually.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> building frontend + desktop binary"
pnpm build >/dev/null
pnpm tauri build --no-bundle

echo "==> building sidecar"
pnpm server:prepare

echo "==> installing (needs sudo)"
sudo install -m 0755 src-tauri/target/release/dshbox /usr/bin/dshbox
sudo install -m 0755 src-tauri/resources/server/linux-x64/dshboxd \
  /usr/lib/dshbox/server/linux-x64/dshboxd
sudo rm -rf /usr/lib/dshbox/runtime/linux-x64
sudo cp -r src-tauri/resources/runtime/linux-x64 /usr/lib/dshbox/runtime/linux-x64

echo "==> done"
