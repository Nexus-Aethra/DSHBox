---
name: dsh-box-tools
description: Use when you need to inspect, start, or change DSH Box containers, templates, plugins or resources from inside a container session — cover box_overview, box_lifecycle, box_create, box_resources, box_plugins, box_templates, box_settings, box_task and the browser page tools.
---

# Driving DSH Box with the `box_*` tools

You run inside a DSH Box container, and this profile mounts the sandbox plugin
(`@nexus-aethra/dsh-box-sandbox`) that bridges to the `dshboxd` daemon on the host.
It registers a typed `box_*` tool surface, so you never have to shell out to `dshbox`
or parse its stdout. Every tool is one JSON call; every answer is the daemon's own
result, not a summary.

## Read this first

- **`box_overview` before anything else.** It is the only listing tool: containers,
  templates, and the plugins the box knows. Acting on an id you guessed is the
  failure this tool removes.
- **Check `self` in the same `box_overview` answer.** It says where you sit:
  `role: container` with the container id you are running in, or `external` when
  you watch the box from outside (a desktop or web DSH). A `container` agent
  must not stop, restart, rebuild or remove its own container — `box_lifecycle`
  refuses, because the call could never report back.
- **Anything that changes state through a task needs `box_task`.** Call it with
  `action: 'status'` and the task id the action returned, or `action: 'list'` to
  find what is already running. A task that has not finished has not happened.
- **A call that disappears is an outage, not an answer.** When the daemon is down
  every tool fails with a message naming the discovery record it looked for; that
  means "the box is not running", not "the thing does not exist".

## The tool surface

| Tool | What it does |
|---|---|
| `box_overview` | Containers, templates, plugins, and where you sit (`self`). Start here. |
| `box_lifecycle` | start / stop / stop-now / restart / rebuild / remove a container |
| `box_create` | from-template, from-version, describe, url, browse-paths |
| `box_resources` | list, read, write, extract, inject, rm, rm-view, view-on, view-off, prune, types, views |
| `box_plugins` | list, installed, container, add, add-extension, copy, remove-plugin, import, export, export-installed, import-workspace, install-bundle, bundle, prune, graph |
| `box_templates` | list, show, pull, import, export, remove, prune, catalog |
| `box_settings` | toolchains, installed, catalog, refresh-catalog, data, references, template-info, template-list, save-mirrors, set-runtime-directory, uninstall-dsh |
| `box_task` | list, status, cancel, delete |
| `box_workspace` | list, add (restart: true stops the container, adds, starts it again) |
| `box_build` | build a template from a boxfile; apply a resource document |

## Rules that are easy to get wrong

1. **This container cannot be restarted from inside it.** `box_lifecycle` refuses
   when the target is the container you are running in — stopping it kills the host
   that is executing your call, so the result can never come back. Use `box_task`
   to watch, and let a person or the CLI act on that container.
2. **`add`, `import` and `pull` are tasks.** They return a task id, not a finished
   object. `box_task` with `action: 'status'` is how you learn whether it worked;
   the task's `logPath` is where the daemon writes the step-by-step log.
3. **Closing a resource type is not deleting it.** `box_resources` with
   `view-off` hides the type's tab and keeps its row and every copy taken under it;
   `rm-view` removes the type itself and leaves the copies alone. Choose deliberately.
4. **Injection refuses rather than overwrites.** `inject` will not replace an
   existing destination without `overwrite`, and will not touch a running container
   without `restart`. That refusal is the safety property, not an error to work around.
5. **Three actions change the whole box and need `confirm: true`.**
   `save-mirrors`, `set-runtime-directory`, `uninstall-dsh` affect every container
   and every future session. The tool refuses without the flag.

## Debugging a page (separate tool family)

The same plugin also registers browser tools against a headless Chromium session:
`box_page_text`, `box_click_element`, `box_click_at`, `box_hover`, `box_type_text`, `box_press_key`,
`box_scroll`, `box_navigate`, `box_evaluate`, `box_screenshot`, `box_console`, `box_set_viewport`,
`box_set_browser`, `box_close`.

When using them, always start with the observation tool and act only on what it
returned. The tool descriptions carry the full workflow; follow them.

## Worked example: take a resource copy and check it landed

1. `box_overview` -> find the container id, e.g. `container-<timestamp>`.
2. `box_resources` with `action: 'list'` and that `containerId` -> see which kinds
   exist and where.
3. `box_resources` with `action: 'extract'`, `kind: 'sessions'` -> returns a task id.
4. `box_task` with `action: 'status'`, that id -> `succeeded` means the copy exists.
5. `box_resources` with `action: 'stored'` -> confirm the new copy is listed.

If a step fails, the daemon's message names what it refused and why. Read it; do not
retry with a different flag to see if that works.
