# `boxfile-plugin-chains.dsh` — every `ADD plugin` spec shape

This boxfile is the canonical reference for what `ADD plugin <src>`
accepts. Each ADD line uses a different source spec so the build
pipeline (`parse → fetch → npm pack → import into repository →
workspace install`) exercises every branch in
`box_image::parse_source_token` and `dshboxd::install_container_plugin`.

## Run it

The boxfile uses **five real public packages** with no duplicates

```bash
# 1. Pull the harness once.
dshbox pull template github.com/deepseek-ai/deepseek-harness:latest

# 2. Prepare the local placeholders so ADD lines 7 and 8 resolve.
mkdir -p /tmp/dsh-local-plugin-placeholder
cat > /tmp/dsh-local-plugin-placeholder/package.json <<'EOF'
{ "name": "dsh-local-plugin-placeholder", "version": "0.0.0", "main": "index.js" }
EOF
echo 'module.exports = {};' > /tmp/dsh-local-plugin-placeholder/index.js
echo "fake tarball for shape test" > /tmp/dsh-archive-placeholder.tar.gz
# (real builds need a real npm-pack-shaped tarball, e.g. one from
#  `npm pack` of an existing plugin)

# 3. Build the template.
dshbox build ./boxfile-plugin-chains.dsh --name dshbox-plugin-chains

# 4. Run it.
dshbox run dshbox-plugin-chains
```

## What every line exercises

| # | Source | `ParsedSource` variant | Builder code path |
|---|---|---|---|
| 1 | `github.com/<owner>/<repo>` (no ref) | `Github { ref: None }` | `git clone` the default branch, npm-pack-style tarball extracted into repo |
| 2 | `github.com/<owner>/<repo>@<ref>` | `Github { ref: Some(...) }` | same as #1, then `git checkout <ref>` before pack |
| 3 | `git:github.com/<owner>/<repo>:<tag>` | `GitPrefix { ref_: ... }` | explicit git intent; equivalent to #1/#2 but unambiguous to reviewers |
| 4 | `github:<owner>/<repo>#<ref>` | `Passthrough { spec: ... }` | pnpm-style prefix; pnpm itself parses it (`github:owner/repo#v1`) and we forward verbatim |
| 5 | `npm:<pkg>@<ver>` | `NpmPrefix { spec: ... }` | `npm pack <spec>` — registry tarball, no clone |
| 6 | `<alias>@npm:<pkg>@<ver>` | `Passthrough { spec: ... }` | npm alias grammar; pnpm unpacks the real package under the alias name |
| 7 | `/abs/path` | `LocalDir { path: ... }` | bare-directory import — no npm pack round-trip; `package.json` is read in place |
| 8 | `file:///abs/path.tar.gz` | `Tarball { url, local: true }` | `npm pack file:...` or direct read; npm-pack-shaped archive required |
| 9 | `https://host/path.tgz` | `Tarball { url, local: false }` | HTTP GET, npm-pack-shaped archive required |
| 10 | `bare-name` | `BareName` | repository lookup — assumes the source was registered earlier via `dshbox plugin import` |

For comparison, `boxfile.dsh` (in the same directory) is a minimal
"real-world" boxfile with three plugins; this one is a shape test.

## Where to look in the source

- **Parser**: `crates/box-image/src/script.rs::parse_source_token`
  is the single source of truth for what `ADD plugin <src>` accepts.
  Adding a new prefix there is the only code change needed to
  support a new spec shape.

- **Builder**: `crates/dshboxd/src/image.rs` routes each
  `ParsedSource` variant to the right fetcher — local dir import,
  `npm pack`, git clone, tarball unpack, etc.

- **Workspace promotion**: `crates/dshboxd/src/bundles.rs::install_container_plugin`
  rewrites `link:` → `workspace:*` and injects `dangerouslyAllowAllBuilds: true`
  into the profile's `pnpm-workspace.yaml` so transitive deps hoist
  and native modules (`node-pty`, `ssh2`, `cpu-features`) build without
  manual `pnpm approve-builds`.

- **Daemon reconcile**: `crates/dshboxd/src/host.rs` describes the
  durable record (`host.json`) and CAS generation logic that keeps
  the watcher thread from clobbering fresh state.

## What you'll see in the task log

For each plugin, in order:

1. `parsing boxfile` (image-build stage).
2. `importing plugin <name> from <spec>` (parser dispatched to the
   right fetcher).
3. `... extract` (npm pack / git clone / tarball unpack).
4. `adding plugin <name> to profile web` (container-build stage).
5. `installing plugin dependencies for <name>` (post-rewrite
   `pnpm install` in the profile dir).
6. `Scope: all N workspace projects / Packages: +M` — the workspace
   hoist output.

A green run means every spec shape was parsed, fetched, and
re-homed into the workspace without manual intervention.

## Known limits

- **One spec shape per source, not per boxfile.** You can mix all
  ten in the same template — the parser tokenises each line
  independently. There's no precedence or fallback.

- **The remote tarball line (`https://example.com/...tgz`) must
  resolve to a real, npm-pack-shaped archive** (`npm pack` output
  or equivalent). A 404 fails the build; a wrong-shape archive
  produces a clear error from `extract_extension_tarball`.

- **Local tarballs are read by path at build time.** They are NOT
  snapshotted into the template's content-addressed store the way
  npm registry downloads are. If you want reproducibility across
  machines, prefer `npm:` or `git:`.

- **Bare names depend on `dshbox plugin import`** having been run
  once before. The build will fail with "extension not in
  repository" if the name isn't already registered.