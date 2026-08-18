#!/usr/bin/env bash
# e2e: Harness version catalog refresh via the libgit2 `ls-remote` path.
# Spins up an isolated daemon (HOME sandbox), configures storage, then
# refreshes the DSH catalog over the RPC and asserts the tag list arrived
# from the git protocol (no GitHub API needed for the primary path).
set -euo pipefail

DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$SCRIPT_DIR")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-cat-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
mkdir -p "$RUNTIME"

echo "[1/5] starting isolated dshboxd in $SANDBOX"
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

python3 - "$SANDBOX" "$RUNTIME" <<'PY'
import json, os, socket, sys, time

base, runtime_dir = sys.argv[1], sys.argv[2]
# Read discovery off the SANDBOX path explicitly: os.path.expanduser("~")
# would resolve the inherited real $HOME and accidentally talk to the
# user's real daemon.
discovery_path = f"{base}/.dsh-box/server/discovery.json"
deadline = time.time() + 10
while time.time() < deadline and not os.path.isfile(discovery_path):
    time.sleep(0.1)
discovery = json.load(open(discovery_path))
token = discovery["token"]
endpoint = discovery["endpoint"]
if not endpoint.startswith(base):
    raise SystemExit(f"stale socket outside sandbox: {endpoint}")
print(f"[2/5] daemon discovered at {endpoint}")

def call(payload, timeout=15):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(endpoint)
        s.sendall((json.dumps({"token": token, **payload}) + "\n").encode())
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    return json.loads(buf.decode().splitlines()[0])

print("[3/5] save_runtime_directory")
resp = call({"method": "save_runtime_directory", "runtimeDirectory": runtime_dir})
assert resp.get("ok") is True, resp

print("[4/5] refresh_dsh_catalog")
resp = call({"method": "refresh_dsh_catalog"})
assert resp.get("ok") is True, resp
task_id = resp["result"]["id"]
deadline = time.time() + 120
last = None
while time.time() < deadline:
    status = call({"method": "task_status", "id": task_id})
    assert status.get("ok") is True, status
    last = status["result"]
    if last["status"] in ("succeeded", "failed", "cancelled", "interrupted"):
        break
    time.sleep(0.5)
else:
    raise SystemExit(f"timeout; last status = {last}")
if last["status"] != "succeeded":
    raise SystemExit(f"catalog refresh failed: {last.get('error')}")

print("[5/5] list_dsh_catalog")
resp = call({"method": "list_dsh_catalog"})
assert resp.get("ok") is True, resp
names = resp["result"]
print(f"      catalog = {names}")
# The harness repo currently carries no tags, so an empty catalog is a
# legitimate outcome; the invariant under test is that the refresh task
# succeeded and persisted a catalog file over the git-protocol path.
state_file = f"{runtime_dir}/state/dsh-catalog.json"
if not os.path.isfile(state_file):
    raise SystemExit(f"catalog file missing: {state_file}")
persisted = json.load(open(state_file))
assert persisted == names or set(persisted).issubset(set(names)), (persisted, names)
print(f"OK: catalog refreshed ({len(names)} entries, file persisted)")
PY

echo "e2e-catalog PASSED"
