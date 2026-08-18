---
name: dsh-container-skill-injection
description: How DSH Box ships container-internal skills (SKILL.md) into the DSH runtime that runs inside each container, and how to add your own via boxfile.dsh.
---

# Container skill injection in DSH Box

When a DSH Box container starts, the DSH process running inside it
loads Cordis-format skills from `$DSH_HOME/skills/`. The DSH agent
inside that container reads them exactly like any other skill — so a
DSH Box container ships with curated domain knowledge the moment it
boots, with no user setup.

## Where skills come from

DSH Box manages `$DSH_HOME` for the DSH child process and points it
at the container's `profile/` directory. There are two skill sources
that get materialised there:

1. **Bundled `boxfile-guide` skill** — written on every container
   creation by `containers.rs::write_boxfile_guide_skill`. It
   explains the boxfile DSL (`FROM`, `PROFILE`, `NAME`, `ADD plugin
   /skill /data`) so the in-container agent can answer questions
   about how to author one. Idempotent: if the file already exists
   (user edited it), DSH Box leaves it alone.

2. **User-authored skills** declared in the boxfile with `ADD skill
   <source>`. The build step snapshots them into the template's
   `profile/skills/<name>/`; the run step copies them into each
   container instance.

## Lifecycle of an `ADD skill` declaration

```
boxfile.dsh       ADD skill my-tool from ./my-tool/   # source path
   │
   ▼ parse_script              (containers.rs)
   │
   ▼ dshbox build              (image.rs)
   template/profile/skills/my-tool/SKILL.md    (committed into template cache)
   template/extensions.json    records the skill install
   │
   ▼ dshbox run                (containers.rs + bundles.rs::install_container_skill)
   container/profile/skills/my-tool/SKILL.md  (per-instance copy)
   │
   ▼ DSH host starts           (lifecycle.rs sets DSH_HOME = container/profile)
   Cordis loader scans DSH_HOME/skills/*   ← in-container agent sees `my-tool`
```

## Where to read this from in the codebase

| Step | File |
|---|---|
| Boxfile parsing | `crates/dshboxd/src/containers.rs` |
| Skill install (build + run) | `crates/dshboxd/src/bundles.rs::install_container_skill` |
| Bundled boxfile-guide write | `crates/dshboxd/src/containers.rs::write_boxfile_guide_skill` |
| DSH_HOME wiring | `crates/dshboxd/src/lifecycle.rs` |
| SKILL.md frontmatter parser | `crates/dshboxd/src/bundles.rs::skill_name` |
| Skill plugin detection (transfer) | `crates/box-extensions/src/transfer.rs` |

## What an `ADD skill` actually copies

- **Source**: a directory with a top-level `SKILL.md` (and optional
  resource files alongside it). Plugin-style packages (with
  `package.json` instead of `SKILL.md`) get rejected by
  `bundles.rs::skill_name` and routed to `install_container_plugin`
  instead.
- **Destination**: `<container>/profile/skills/<frontmatter-name>/`
  where `<frontmatter-name>` is the `name:` field of the SKILL.md
  YAML frontmatter (or the directory name as fallback).
- **Conflict**: if the destination already exists, the install
  fails with `skill already exists: <name>`. Boxfiles do **not**
  overwrite by design — the user owns the per-container copy.

## Authoring a custom skill for a container

1. Create a directory with `SKILL.md` whose frontmatter sets a
   safe `name:` (letters, digits, dots, dashes, underscores only —
   enforced by `bundles.rs::is_safe_identifier`).
2. Reference it from the boxfile:
   ```dsh
   ADD skill my-tool from ./my-tool/
   ```
3. Build (`dshbox build`) and run (`dshbox run`). The in-container
   agent will see `my-tool` in its skill catalogue immediately.

## Differences vs. `ADD plugin`

| | `ADD skill` | `ADD plugin` |
|---|---|---|
| Format | `SKILL.md` + resources | `package.json` (Cordis plugin) |
| Install path | `profile/skills/<name>/` | profile's `node_modules/<name>/` |
| Scope | per-container copy | profile workspace member |
| Runtime | DSH reads at startup, exposes to agent prompt | DSH loads as Cordis plugin (CLI flags, services) |
| Hot-reload | No (requires container rebuild) | No (requires rebuild) |

They are not interchangeable: a Cordis plugin gets loaded into the
DSH plugin graph; a skill gets read by the agent as instructions.

## Operational caveats

- The bundled `boxfile-guide` skill is written only if `SKILL.md`
  is missing — users can edit it freely and DSH Box will not
  clobber their changes.
- A skill installed with the same `name:` twice in one boxfile (or
  once in boxfile and once via `dshbox plugin skill install`)
  causes `install_container_skill` to return `skill already
  exists`. Resolve by `dshbox plugin skill rm <name>` before
  rebuilding.
- Skills ship **per container**, not per DSH version. Rebuilding the
  DSH harness does not retouch them.