#!/usr/bin/env bash
# end-to-end check: create a container via the daemon RPC and assert the
# built-in `boxfile-guide` skill was dropped into <container>/profile/skills/
# so first-time users can immediately read boxfile syntax from inside the
# container.
set -euo pipefail

DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$SCRIPT_DIR")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-skill-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
export HOME="$SANDBOX"
mkdir -p "$RUNTIME"

echo "[1/7] starting isolated dshboxd in $SANDBOX"
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
home = os.path.expanduser("~")
discovery = json.load(open(f"{home}/.dsh-box/server/discovery.json"))
token = discovery["token"]
endpoint = discovery["endpoint"]
print(f"[2/7] daemon discovered at {endpoint}")

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

print("[3/7] save_runtime_directory")
resp = call({"method": "save_runtime_directory", "runtimeDirectory": runtime_dir})
assert resp.get("ok") is True, resp

print("[4/7] pull_template github.com/deepseek-ai/deepseek-harness:latest")
resp = call({"method": "pull_template", "ref": "github.com/deepseek-ai/deepseek-harness:latest"})
assert resp.get("ok") is True, resp
task_id = resp["result"]["id"]

deadline = time.time() + 180
last = None
while time.time() < deadline:
    status = call({"method": "task_status", "id": task_id})
    assert status.get("ok") is True, status
    last = status["result"]
    if last["status"] in ("succeeded", "failed", "cancelled", "interrupted"):
        break
    time.sleep(0.5)
assert last["status"] == "succeeded", f"pull failed: {last}"

print("[5/7] create_container (sync RPC, no DSH startup)")
resp = call({
    "method": "create_container",
    "name": "test-container",
    "version": "latest",
    "profile": "web",
})
assert resp.get("ok") is True, resp
print(f"      container id = {resp['result']['id']}")

print("[6/7] verify skill landed in the container directory")
container_dir = resp["result"]["directory"]

print("[7/7] assert boxfile-guide skill is present")
skill_md = os.path.join(container_dir, "profile", "skills", "boxfile-guide", "SKILL.md")
assert os.path.isfile(skill_md), f"boxfile-guide SKILL.md missing at {skill_md}"
content = open(skill_md).read()
assert "name: boxfile-guide" in content
assert "FROM <base>" in content
assert "ADD <kind>" in content
assert "## Best practices" in content
assert "dshbox help" in content
print(f"OK: boxfile-guide skill installed at {skill_md}")
PY
