---
title: Starting a container opened the user's real browser, and process counting could not see it
date: 2026-10-03
---

# Symptom

Starting, restarting or rebuilding a container popped the user's default browser and took the
foreground with it. Reported more than once.

# What made it hard to find

The obvious check is to count browser processes before and after a start. That check reports
nothing, and it reports nothing *correctly*:

- **A browser that is already running is handed the URL, not started.** No new process exists,
  so a before/after process count cannot see a tab opening in a browser that was already up.
  This was the mistake in the first investigation -- the count came back zero and was read as
  "nothing opened", when what had happened was that the tab opened inside an existing Edge.
- **Child processes do not repeat the parent's flags.** A renderer started under a headless
  browser still has `--user-data-dir` but neither `--headless` nor the debugging port in its
  own command line, so grouping by flags marks the whole tree as headed.
- **The spawn sits in a retry loop** (`HOST_BIND_ATTEMPTS`), so one start could pop the browser
  up to three times, which makes it look intermittent.

# Actual cause

`dshboxd/src/lifecycle.rs` spawned the DSH host without `--no-open`. DSH's web command opens the
default browser unless told not to, and says so in its own log:

    dsh web: opening the default browser; pass --no-open to disable

Reachable from `container-start`, `container-restart` and `container-rebuild`, and from a
resource inject with restart. Container *create* does not spawn the host, which is why creating
one appeared to be clean.

# Fix

Pass `--no-open` in the host's argv. A Box container is a server: its UI is a webview the user
asks for, and the sandbox agent drives it over loopback. Nothing about running a container
should take over the desktop.

# How to actually check this class of thing

Read the host log, not the process table. `<container>/logs/host.log` names the behaviour in
the program's own words, which is both faster and unambiguous.
