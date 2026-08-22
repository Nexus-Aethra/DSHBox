#!/usr/bin/env bash
# end-to-end check: prove that the bundled Git distribution is reachable
# and runnable from a clean-room pnpm child. The test skips itself when
# the bundled runtime is missing or when git is not bundled (e.g. on Linux
# until DSH Box CI produces a private bundle). Use this to guard against
# regressions in `bundled_package_manager_policy` after touching the
# clean-room environment code.
#
# This script only exercises the file-system + executable side of the
# bundling. The clean-room policy itself (PATH prepend + GIT_* injection)
# is covered by `bundled_package_manager_policy_injects_git_path_and_clean_room_vars`
# in `src-tauri/crates/box-runtime/src/process/env.rs`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

case "$(uname -s)" in
  Linux*) os="linux" ;;
  Darwin*) os="macos" ;;
  MINGW*|MSYS*|CYGWIN*) os="win" ;;
  *) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64) arch="x64" ;;
  aarch64|arm64) arch="arm64" ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
TARGET="${os}-${arch}"
RUNTIME_ROOT="$REPO_ROOT/src-tauri/resources/runtime/$TARGET"
MANIFEST="$RUNTIME_ROOT/runtime-manifest.json"

if [ ! -f "$MANIFEST" ]; then
  echo "skipping: runtime manifest not found at $MANIFEST; run \`pnpm runtime:prepare\` first"
  exit 0
fi

GIT_ENTRY="$(sed -n 's/.*"gitEntry"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST")"
if [ -z "$GIT_ENTRY" ]; then
  echo "skipping: bundled git is not configured for target $TARGET"
  exit 0
fi

# gitEntry is the path relative to the git/ subdirectory.
GIT_EXE="$RUNTIME_ROOT/git/$GIT_ENTRY"
if [ ! -f "$GIT_EXE" ]; then
  echo "FAIL: manifest declares bundled git at $GIT_EXE but the file does not exist"
  exit 1
fi

# Sanity check: bundled git reports the same version the lock pinned.
EXPECTED_VERSION="$(sed -n 's/.*"gitVersion"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST")"
ACTUAL_VERSION="$("$GIT_EXE" --version | awk '{print $3}')"
if [ "$ACTUAL_VERSION" != "$EXPECTED_VERSION" ]; then
  echo "FAIL: bundled git reports $ACTUAL_VERSION but lock pins $EXPECTED_VERSION"
  exit 1
fi

# Sanity check: bundled git honors `git -c <key>=<value>` invocation. This
# is the exact path pnpm's `git ls-remote` uses when a clean-room child
# overrides `GIT_CONFIG_GLOBAL` and pnpm wants to set extra keys. A failure
# here means the portable distribution is missing a helper that pnpm's
# `git ls-remote` will also need.
PROBE_OUTPUT="$("$GIT_EXE" -c test.probe=ok config --get test.probe)"
if [ "$PROBE_OUTPUT" != "ok" ]; then
  echo "FAIL: bundled git did not honor -c override: $PROBE_OUTPUT"
  exit 1
fi

echo "ok: bundled git $ACTUAL_VERSION is runnable and accepts -c overrides"
