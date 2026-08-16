#!/usr/bin/env bash
# e2e: full build → run pipeline for a built template (regression for
# the 2026-08-17 bugs where dshbox build produced a metadata-only
# built template that dshbox run failed to start due to a stale
# `template_content_path` lookup).
#
# Runs in an isolated $HOME sandbox so it never touches the user's
# real runtime directory. Requires the dshboxd / dshbox release
# binaries at $DSHBOXD / $DSHBOX (default: src-tauri/target/release).
set -euo pipefail

DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
DSHBOX="${DSHBOX:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshbox}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$REPO_ROOT")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-buildrun-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
mkdir -p "$RUNTIME"

if [ ! -x "$DSHBOXD" ] || [ ! -x "$DSHBOX" ]; then
  echo "FATAL: build the workspace first: cargo +stable build --release -p dshboxd -p dshbox"
  exit 2
fi

echo "[1/8] starting dshboxd in $SANDBOX"
HOME="$SANDBOX" "$DSHBOXD" > "$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; sleep 0.2; rm -rf "$SANDBOX"' EXIT

# Wait for the daemon to write its discovery file.
for _ in $(seq 1 50); do
  [ -s "$SANDBOX/.dsh-box/server/discovery.json" ] && break
  sleep 0.1
done
if [ ! -s "$SANDBOX/.dsh-box/server/discovery.json" ]; then
  echo "FAIL: discovery.json not created"
  cat "$SANDBOX/daemon.log"
  exit 1
fi

echo "[2/8] config set runtime + pull template"
HOME="$SANDBOX" "$DSHBOX" config set runtime "$RUNTIME" > /dev/null
HOME="$SANDBOX" "$DSHBOX" pull template github.com/deepseek-ai/deepseek-harness:latest

echo "[3/8] write a boxfile that names the built template"
SANDBOX_PROJECT="$SANDBOX/project"
mkdir -p "$SANDBOX_PROJECT"
cat > "$SANDBOX_PROJECT/boxfile.dsh" <<'EOF'
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME dsh-test
ADD plugin github.com/omdsh-dev/DSH-better-sidebar:v0.12.3
EOF

echo "[4/8] dshbox build -- must produce a built template, NOT a container"
cd "$SANDBOX_PROJECT"
HOME="$SANDBOX" "$DSHBOX" build ./boxfile.dsh --name dsh-test | tail -2
cd - > /dev/null

# `build` is metadata-only; there must be no container yet.
home_template_index="$RUNTIME/state/template-index.json"
if ! grep -q '"dsh-test"' "$home_template_index"; then
  echo "FAIL: dsh-test not registered in $home_template_index"
  cat "$RUNTIME/state/template-index.json"
  exit 1
fi
container_count="$(HOME="$SANDBOX" "$DSHBOX" ps | tail -n +2 | wc -l)"
if [ "$container_count" -ne 0 ]; then
  echo "FAIL: build leaked a container (ps shows $container_count rows)"
  HOME="$SANDBOX" "$DSHBOX" ps
  exit 1
fi

echo "[5/8] write a second boxfile with the same plugin to exercise cache hit"
SANDBOX_PROJECT2="$SANDBOX/project2"
mkdir -p "$SANDBOX_PROJECT2"
cat > "$SANDBOX_PROJECT2/boxfile.dsh" <<'EOF'
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME dsh-test2
ADD plugin github.com/omdsh-dev/DSH-better-sidebar:v0.12.3
EOF
cd "$SANDBOX_PROJECT2"
HOME="$SANDBOX" "$DSHBOX" build ./boxfile.dsh --name dsh-test2 | tail -2
cd - > /dev/null

# Plugin cache hit: only one dsh-better-sidebar entry should exist in the
# repository, regardless of how many templates use it.
plugin_count="$(HOME="$SANDBOX" "$DSHBOX" plugin ls | grep -c dsh-better-sidebar || true)"
if [ "$plugin_count" -ne 1 ]; then
  echo "FAIL: plugin cache miss — expected 1 dsh-better-sidebar entry, saw $plugin_count"
  HOME="$SANDBOX" "$DSHBOX" plugin ls
  exit 1
fi

echo "[6/8] dshbox run dsh-test -- must materialise the built template and start the DSH host"
if ! HOME="$SANDBOX" "$DSHBOX" run dsh-test --name dsh-test-runtime 2>&1 | tee "$SANDBOX/run.log" | tail -5; then
  echo "FAIL: dshbox run exited non-zero"
  exit 1
fi
# The bug we are guarding against was the inner `template not found`
# trail; assert neither half of the wrapped error appears.
if grep -q "template not found" "$SANDBOX/run.log"; then
  echo "FAIL: run produced a 'template not found' error message"
  exit 1
fi

echo "[7/8] ps: the container must be running"
ps_output="$(HOME="$SANDBOX" "$DSHBOX" ps)"
echo "$ps_output"
if ! echo "$ps_output" | grep -q "dsh-test-runtime.*running"; then
  echo "FAIL: dsh-test-runtime is not running"
  exit 1
fi

echo "[8/8] curl the webview URL"
container_url="$(HOME="$SANDBOX" "$DSHBOX" container url dsh-test-runtime 2>/dev/null || true)"
if [ -z "$container_url" ]; then
  # Fall back to extracting the URL from the run log.
  container_url="$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$SANDBOX/run.log" | tail -1)"
fi
if [ -z "$container_url" ]; then
  echo "FAIL: cannot resolve container URL"
  exit 1
fi
status="$(curl -s -o /dev/null -w "%{http_code}" -m 8 "$container_url" || true)"
if [ "$status" != "200" ]; then
  echo "FAIL: webview returned HTTP $status for $container_url"
  exit 1
fi
echo "OK: build → run → HTTP 200 from $container_url"
