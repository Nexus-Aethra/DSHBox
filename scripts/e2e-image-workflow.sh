#!/usr/bin/env bash
# e2e: image build pipeline (docs/specs/image-build.md).
#
# Verifies the agreed architecture end to end in an isolated daemon:
#   1. build produces a METADATA-ONLY image (no container)
#      - plugins recorded as repository references
#      - skill/data recorded as hash snapshots of the data store
#   2. `image ls` shows the registry entry
#   3. run <image> creates a container where
#      - the plugin is LINKED from the repository
#      - the skill snapshot is HARD-COPIED into profile/skills/
#      - the data snapshot is HARD-COPIED into extensions/data/
set -euo pipefail

DSHBOXD="${DSHBOXD:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshboxd}"
DSHBOX="${DSHBOX:-/home/wpp/homework/DSHBox/src-tauri/target/release/dshbox}"
REF="${REF:-github.com/deepseek-ai/deepseek-harness:latest}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH_PARENT="${SCRATCH_PARENT:-$(dirname "$SCRIPT_DIR")/.tmp}"
mkdir -p "$SCRATCH_PARENT"
SANDBOX="$(mktemp -d "$SCRATCH_PARENT/dsh-e2e-img-XXXXXX")"
RUNTIME="$SANDBOX/runtime"
mkdir -p "$RUNTIME"
export HOME="$SANDBOX"

echo "[1/9] starting isolated dshboxd in $SANDBOX"
HOME="$SANDBOX" "$DSHBOXD" > "$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true; sleep 0.2; rm -rf "$SANDBOX"' EXIT

for _ in $(seq 1 50); do
  if [ -s "$SANDBOX/.dsh-box/server/discovery.json" ]; then break; fi
  sleep 0.1
done
[ -s "$SANDBOX/.dsh-box/server/discovery.json" ] || { echo "FAIL: discovery.json not created"; cat "$SANDBOX/daemon.log"; exit 1; }

echo "[2/9] config set runtime + pull template"
HOME="$SANDBOX" "$DSHBOX" config set runtime "$RUNTIME"
HOME="$SANDBOX" "$DSHBOX" pull template "$REF" 2>&1 | tail -1

echo "[3/9] writing boxfile (plugin github + skill local + data local)"
mkdir -p "$SANDBOX/src/demo-skill" "$SANDBOX/src/demo-data"
cat > "$SANDBOX/src/demo-skill/SKILL.md" <<'MD'
---
name: demo-skill
---
Demo skill snapshot.
MD
echo "demo data payload" > "$SANDBOX/src/demo-data/corpus.txt"

cat > "$SANDBOX/boxfile.dsh" <<DSH
FROM $REF
PROFILE web
NAME image-e2e
VERSION latest

ADD plugin github.com/omdsh-dev/DSH-better-sidebar
ADD skill $SANDBOX/src/demo-skill
ADD data $SANDBOX/src/demo-data
DSH

echo "[4/9] build -> must produce an IMAGE, not a container"
HOME="$SANDBOX" "$DSHBOX" build "$SANDBOX/boxfile.dsh" --name image-e2e 2>&1 | tail -2

python3 - "$SANDBOX" <<'PY'
import json, os, sys

sandbox = sys.argv[1]
runtime = f"{sandbox}/runtime"
# No container may exist after a pure build.
instances = f"{runtime}/instances"
created = os.listdir(instances) if os.path.isdir(instances) else []
assert created == [], f"build created containers: {created}"
# The registry must hold exactly our image with the classified resources.
index = json.load(open(f"{runtime}/state/image-index.json"))
assert "image-e2e" in index, f"image missing from index: {index}"
entry = index["image-e2e"]
listing = json.load(open(f"{runtime}/images/{entry['id']}/list.json"))
modes = {r["name"]: r["mode"] for r in listing["resources"]}
assert modes.get("dsh-better-sidebar") == "reference", modes
assert modes.get("demo-skill") == "snapshot", modes
assert modes.get("demo-data") == "snapshot", modes
# Snapshots must exist in the data store.
for r in listing["resources"]:
    if r["mode"] == "snapshot":
        assert os.path.isdir(f"{runtime}/data/{r['digest']}"), r
print("OK: build produced a metadata-only image (reference + 2 snapshots)")
PY

echo "[5/9] image ls / show"
HOME="$SANDBOX" "$DSHBOX" image ls
HOME="$SANDBOX" "$DSHBOX" image show image-e2e | head -8

echo "[6/9] run image-e2e (create from image + start)"
HOME="$SANDBOX" "$DSHBOX" run image-e2e --name image-e2e-app 2>&1 | tail -2

echo "[7/9] verify materialisation in the container"
python3 - "$SANDBOX" <<'PY'
import json, os, sys

sandbox = sys.argv[1]
runtime = f"{sandbox}/runtime"
instances = os.listdir(f"{runtime}/instances")
assert len(instances) == 1, f"expected one container, got {instances}"
container = f"{runtime}/instances/{instances[0]}"
meta = json.load(open(f"{container}/container.json"))
assert meta.get("image") == "image-e2e", meta

# plugin: LINKED from the repository (symlink, not a copy)
plugin_dir = f"{container}/profile/profiles/web/node_modules/dsh-better-sidebar"
assert os.path.islink(plugin_dir) or os.path.isdir(plugin_dir), "plugin not installed"
assert os.path.islink(plugin_dir), "plugin must be a repository link, not a copy"

# skill snapshot: HARD-COPIED (real directory, detached from the store)
skill_dir = f"{container}/profile/skills/demo-skill"
assert os.path.isdir(skill_dir) and not os.path.islink(skill_dir), "skill must be a hard copy"
assert os.path.isfile(f"{skill_dir}/SKILL.md")

# data snapshot: HARD-COPIED under extensions/data/
data_dir = f"{container}/extensions/data/demo-data"
assert os.path.isdir(data_dir) and not os.path.islink(data_dir), "data must be a hard copy"
assert open(f"{data_dir}/corpus.txt").read().strip() == "demo data payload"
print("OK: plugin linked, skill + data hard-copied, container.json records the image")
PY

echo "[8/9] image rm must refuse while the container uses it"
if HOME="$SANDBOX" "$DSHBOX" image rm image-e2e 2>/dev/null; then
  echo "FAIL: image rm should have been refused"; exit 1
fi
echo "OK: removal refused while referenced"

echo "[9/9] image prune keeps referenced snapshots"
HOME="$SANDBOX" "$DSHBOX" image prune

echo "e2e-image-workflow PASSED"
