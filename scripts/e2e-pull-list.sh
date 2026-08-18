#!/usr/bin/env bash
# end-to-end check: pull template github.com/deepseek-ai/deepseek-harness:latest
# in an isolated daemon, then list templates and assert the new entry showed
# up. Filesystem is in $TMPDIR/dsh-e2e-$$; daemon listens on a unix socket
# under $HOME/.dsh-box/server. The user's existing daemon is untouched
# because $HOME points at the temp directory.
set -euo pipefail

DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
REF="${REF:-github.com/deepseek-ai/deepseek-harness:latest}"
# Use a working-directory root so the script stays inside the sandbox
# writable area even when `/tmp` is read-only.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$SCRIPT_DIR")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
export HOME="$SANDBOX"
mkdir -p "$RUNTIME"

echo "[1/6] starting isolated dshboxd in $SANDBOX"
HOME="$SANDBOX" "$DSHBOXD" > "$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; sleep 0.2; rm -rf "$SANDBOX"' EXIT

# wait for discovery.json to appear
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

python3 - "$SANDBOX" "$REF" "$RUNTIME" <<'PY'
import json, os, socket, sys, time

base, ref, runtime_dir = sys.argv[1], sys.argv[2], sys.argv[3]
home = os.path.expanduser("~")  # matches $HOME we exported
discovery = json.load(open(f"{home}/.dsh-box/server/discovery.json"))
token = discovery["token"]
endpoint = discovery["endpoint"]
print(f"[2/6] daemon discovered at {endpoint}")

def call(payload, timeout=10):
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

print("[3/6] save_runtime_directory")
resp = call({"method": "save_runtime_directory", "runtimeDirectory": runtime_dir})
assert resp.get("ok") is True, resp

print(f"[4/6] pull_template {ref}")
resp = call({"method": "pull_template", "ref": ref})
assert resp.get("ok") is True, resp
task_id = resp["result"]["id"]
print(f"      task id = {task_id}")

print("[5/6] wait for task to succeed")
deadline = time.time() + 180
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
print(f"      status = {last['status']}")
if last["status"] != "succeeded":
    raise SystemExit(f"pull failed: {last.get('error')}")

print("[6/6] list_templates")
resp = call({"method": "list_templates"})
assert resp.get("ok") is True, resp
templates = resp["result"]
print(f"      {len(templates)} template(s):")
for entry in templates:
    print(f"        - {entry}")

names = [t["name"] for t in templates]
assert any(t["name"] == "github.com/deepseek-ai/deepseek-harness:latest" for t in templates), (
    f"template pulled from {ref!r} did not surface in list_templates; got {names!r}"
)
# The pulled entry must carry the harness tag parsed from the ref so the
# Resources page can render the source of the template.
entry = next(t for t in templates if t["name"].startswith("github.com/deepseek-ai"))
assert entry["harnessRef"] == "latest", entry
assert entry["profile"] == "web", entry
assert entry["id"] and len(entry["id"]) == 16, entry  # fnv1a64 hex

# Verify the hash-addressed layout is in use.
index_path = os.path.join(runtime_dir, "state", "template-index.json")
assert os.path.isfile(index_path), f"index file missing: {index_path}"
index = json.load(open(index_path))
assert "github.com/deepseek-ai/deepseek-harness:latest" in index, (
    "ref name not in template index"
)
hash_dir = os.path.join(runtime_dir, "templates", entry["id"])
assert os.path.isfile(os.path.join(hash_dir, "script.dsh")), (
    f"hash-addressed script missing: {hash_dir}/script.dsh"
)
assert os.path.isfile(os.path.join(hash_dir, "manifest.json")), (
    f"hash-addressed manifest missing: {hash_dir}/manifest.json"
)
manifest = json.load(open(os.path.join(hash_dir, "manifest.json")))
assert manifest["name"] == "github.com/deepseek-ai/deepseek-harness:latest", manifest

print(f"OK: pull-template -> list_templates round trip succeeded")
PY
