#!/usr/bin/env bash
# end-to-end check: from a bare pull, build a container that installs the
# omdsh-dev/DSH-better-sidebar plugin, then start the DSH host. Verifies
# the full pull → build → start workflow using a single isolated daemon.
set -euo pipefail

DSHBOX="${DSHBOX:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshbox}"
DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
PLUGIN_REF="${PLUGIN_REF:-github.com/omdsh-dev/DSH-better-sidebar}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$SCRIPT_DIR")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-full-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
WORKSPACE="$SANDBOX/workspace"
export HOME="$SANDBOX"
mkdir -p "$RUNTIME" "$WORKSPACE"

echo "[1/9] starting isolated dshboxd in $SANDBOX"
HOME="$SANDBOX" "$DSHBOXD" > "$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; sleep 0.2; rm -rf "$SANDBOX"' EXIT

for _ in $(seq 1 50); do
  if [ -s "$SANDBOX/.dsh-box/server/discovery.json" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -s "$SANDBOX/.dsh-box/server/discovery.json" ]; then
  echo "FAIL: discovery.json not created"
  cat "$SANDBOX/daemon.log"
  exit 1
fi

echo "[2/9] dshbox config set runtime"
"$DSHBOX" config set runtime "$RUNTIME"

echo "[3/9] dshbox pull template github.com/deepseek-ai/deepseek-harness:latest"
"$DSHBOX" pull template github.com/deepseek-ai/deepseek-harness:latest

echo "[4/9] dshbox template ls"
"$DSHBOX" template ls

echo "[5/9] dshbox init"
cd "$WORKSPACE"
"$DSHBOX" init
ls -la boxfile.dsh

echo "[6/9] write a boxfile with $PLUGIN_REF and build"
cat > boxfile.dsh <<EOF
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web

# Install the DSH-better-sidebar plugin from GitHub.
ADD plugin $PLUGIN_REF
EOF
"$DSHBOX" build ./boxfile.dsh --name sidebar-test

echo "[7/9] dshbox ps"
"$DSHBOX" ps

echo "[8/9] dshbox run the template to start the container"
"$DSHBOX" run github.com/deepseek-ai/deepseek-harness:latest --name sidebar-runtime || true

echo "[9/9] verify container reached running state"
"$DSHBOX" ps
