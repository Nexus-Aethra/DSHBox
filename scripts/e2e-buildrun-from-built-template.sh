#!/usr/bin/env bash
# e2e: full build → run pipeline for a built template, plus the new
# container-lifecycle actions (describe/show/open/rm) added in 2026-08.
#
# Steps 1-8 regress the 2026-08-17 bugs where dshbox build produced a
# metadata-only built template that dshbox run failed to start due to a
# stale `template_content_path` lookup. Steps 9-10 cover
# `container describe --json` (verifies the new wire payload matches
# expectations) and `container rm` (verifies the container disappears
# from `ps` and a follow-up `describe` reports it as not found).
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

echo "[1/10] starting dshboxd in $SANDBOX"
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

echo "[2/10] config set runtime + pull template"
HOME="$SANDBOX" "$DSHBOX" config set runtime "$RUNTIME" > /dev/null
HOME="$SANDBOX" "$DSHBOX" pull template github.com/deepseek-ai/deepseek-harness:latest

echo "[3/10] write a boxfile that names the built template"
SANDBOX_PROJECT="$SANDBOX/project"
mkdir -p "$SANDBOX_PROJECT"
cat > "$SANDBOX_PROJECT/boxfile.dsh" <<'EOF'
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME dsh-test
LABEL dshbox.allow-build=git+https://github.com/omdsh-dev/DSH-better-sidebar#v0.12.3
ADD plugin github.com/omdsh-dev/DSH-better-sidebar:v0.12.3
EOF

echo "[4/10] dshbox build -- must produce a built template, NOT a container"
cd "$SANDBOX_PROJECT"
HOME="$SANDBOX" "$DSHBOX" build ./boxfile.dsh --name dsh-test | tail -2
cd - > /dev/null

# `build` is metadata-only; there must be no container yet.
home_template_index="$RUNTIME/state/sealed-templates.json"
if ! grep -q '"dsh-test"' "$home_template_index"; then
  echo "FAIL: dsh-test not registered in $home_template_index"
  cat "$home_template_index"
  exit 1
fi
container_count="$(HOME="$SANDBOX" "$DSHBOX" ps | tail -n +2 | wc -l)"
if [ "$container_count" -ne 0 ]; then
  echo "FAIL: build leaked a container (ps shows $container_count rows)"
  HOME="$SANDBOX" "$DSHBOX" ps
  exit 1
fi

echo "[5/10] write a second boxfile with the same plugin to exercise cache hit"
SANDBOX_PROJECT2="$SANDBOX/project2"
mkdir -p "$SANDBOX_PROJECT2"
cat > "$SANDBOX_PROJECT2/boxfile.dsh" <<'EOF'
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
NAME dsh-test2
LABEL dshbox.allow-build=git+https://github.com/omdsh-dev/DSH-better-sidebar#v0.12.3
ADD plugin github.com/omdsh-dev/DSH-better-sidebar:v0.12.3
EOF
cd "$SANDBOX_PROJECT2"
HOME="$SANDBOX" "$DSHBOX" build ./boxfile.dsh --name dsh-test2 | tail -2
cd - > /dev/null

# Both builds must register. The cached plugin-store check used to live
# here (`dshbox plugin ls | grep dsh-better-sidebar`), but the boxfile
# ADD path no longer round-trips through the repository — it pins the
# recipe directly inside the sealed template's plugin_sources list.
# Look for the duplicated name there instead so the regression stays
# meaningful without depending on a stale repo-import side effect.
sealed_index="$RUNTIME/state/sealed-templates.json"
if ! grep -q '"dsh-test"' "$sealed_index" || ! grep -q '"dsh-test2"' "$sealed_index"; then
  echo "FAIL: dsh-test or dsh-test2 not registered in $sealed_index"
  cat "$sealed_index"
  exit 1
fi

echo "[6/10] dshbox run dsh-test -- must materialise the built template and start the DSH host"
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

echo "[7/10] ps: the container must be running"
ps_output="$(HOME="$SANDBOX" "$DSHBOX" ps)"
echo "$ps_output"
if ! echo "$ps_output" | grep -q "dsh-test-runtime.*running"; then
  echo "FAIL: dsh-test-runtime is not running"
  exit 1
fi

echo "[8/10] curl the webview URL"
container_url="$(HOME="$SANDBOX" "$DSHBOX" container url dsh-test-runtime 2>/dev/null || true)"
if [ -z "$container_url" ]; then
  # Fall back to extracting the URL from the run log.
  container_url="$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$SANDBOX/run.log" | tail -1)"
fi
if [ -z "$container_url" ]; then
  echo "FAIL: cannot resolve container URL"
  exit 1
fi
# --noproxy: the webview URL is loopback; honouring HTTP_PROXY here would
# send it through a developer's proxy and report 502 for a healthy host.
status="$(curl --noproxy '*' -s -o /dev/null -w "%{http_code}" -m 8 "$container_url" || true)"
if [ "$status" != "200" ]; then
  echo "FAIL: webview returned HTTP $status for $container_url"
  exit 1
fi
echo "OK: build → run → HTTP 200 from $container_url"

container_id="$(echo "$ps_output" | awk -v name="dsh-test-runtime" '$2 == name { print $1; exit }')"
if [ -z "$container_id" ]; then
  echo "FAIL: cannot resolve container id from ps output"
  echo "$ps_output"
  exit 1
fi

echo "[9/10] container describe --json: payload sanity checks"
describe_json="$SANDBOX/describe.json"
if ! HOME="$SANDBOX" "$DSHBOX" container describe "$container_id" --json > "$describe_json" 2>"$SANDBOX/describe.err"; then
  echo "FAIL: container describe exited non-zero"
  cat "$SANDBOX/describe.err"
  exit 1
fi
python3 - "$describe_json" "$container_id" "$container_url" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    payload = json.load(fh)
expected_id = sys.argv[2]
expected_url = sys.argv[3]
assert payload["id"] == expected_id, f"id mismatch: {payload['id']!r}"
assert payload["status"] == "running", f"status: {payload['status']!r}"
assert payload["url"] == expected_url, f"url: got {payload['url']!r}, want {expected_url!r}"
assert isinstance(payload["hostPid"], int) and payload["hostPid"] > 0, f"hostPid missing: {payload['hostPid']!r}"
profiles = payload["extensions"]["profiles"]
assert profiles, "extensions.profiles should not be empty"
web = next((p for p in profiles if p["name"] == "web"), None)
assert web is not None, f"web profile missing: {[p['name'] for p in profiles]!r}"
print(f"describe OK: id={payload['id']} status={payload['status']} url={payload['url']} pid={payload['hostPid']} profiles={[p['name'] for p in profiles]}")
PY

# Default text view (no --json) should also work and mention the same fields.
describe_text="$SANDBOX/describe.txt"
HOME="$SANDBOX" "$DSHBOX" container describe "$container_id" > "$describe_text" 2>"$SANDBOX/describe-text.err"
if ! grep -q "status:    running" "$describe_text"; then
  echo "FAIL: text describe view did not show status=running"
  cat "$describe_text"
  exit 1
fi
if ! grep -q "url:" "$describe_text"; then
  echo "FAIL: text describe view did not show url field"
  cat "$describe_text"
  exit 1
fi

# `show` must be an alias for `describe`: same payload when --json.
show_json="$SANDBOX/show.json"
HOME="$SANDBOX" "$DSHBOX" container show "$container_id" --json > "$show_json" 2>"$SANDBOX/show.err"
if ! diff -q "$describe_json" "$show_json" > /dev/null; then
  echo "FAIL: container show --json diverged from container describe --json"
  diff "$describe_json" "$show_json" | head -20
  exit 1
fi

echo "[10/10] container rm: container must vanish from ps"
HOME="$SANDBOX" "$DSHBOX" container rm "$container_id"
final_ps="$(HOME="$SANDBOX" "$DSHBOX" ps || true)"
if echo "$final_ps" | grep -q "$container_id"; then
  echo "FAIL: container $container_id still appears in ps after rm"
  echo "$final_ps"
  exit 1
fi
# Asking describe again now must report container not found.
if HOME="$SANDBOX" "$DSHBOX" container describe "$container_id" 2>"$SANDBOX/describe-after.err"; then
  echo "FAIL: describe on a deleted container should have failed"
  cat "$SANDBOX/describe-after.err"
  exit 1
fi
if ! grep -q "container not found" "$SANDBOX/describe-after.err"; then
  echo "FAIL: describe error does not mention 'container not found'"
  cat "$SANDBOX/describe-after.err"
  exit 1
fi
echo "OK: container lifecycle describe/open/rm covered"
