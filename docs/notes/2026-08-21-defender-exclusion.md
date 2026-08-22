# Windows Defender exclusion for DSH Box

## Symptom

`pnpm install --offline` inside a container instance directory (typically
`D:\ddd\instances\container-*/harness/`) fails with:

```
[UNKNOWN] UNKNOWN: unknown error, open '...cytoscape\package.json'
```

or similar `EBUSY`/`EPERM` against random `package.json` files inside
`node_modules/`. The bundled pnpm install completed the 935-package
graph, then died at the second invocation that re-reads the same files
during verification.

## Cause

Windows Defender real-time scan is racing with pnpm's file reads inside
`D:\ddd\instances\*\harness\node_modules\`. Defender scans each newly
written `package.json` as pnpm extracts it, and the next pnpm read
sometimes catches Defender mid-scan. This is not deterministic but
becomes reliable with large workspaces.

## Fix

Register DSH Box's two working directories as Defender exclusion paths.
Idempotent. Requires administrator elevation once.

```powershell
# From an elevated PowerShell on the user's machine:
Add-MpPreference -ExclusionPath "C:\Program Files\DSH Box"
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.dsh-box"
```

`scripts/register-defender-exclusion.ps1` automates the same two calls,
tolerates hosts without Defender (Server Core, third-party AV), and
accepts `-Uninstall` to reverse the change. It is intended to be
shipped next to `dshbox.exe` in the MSI bundle and invoked by the
desktop app on first run via UAC elevation; the manual form above is
the workaround until that wiring lands.

Hosts without Defender (Server Core, custom AV) see `skip: ... host has
no Defender interface` and continue normally.

## Status

The standalone script is in place. Auto-invocation on first run
(elevated UAC prompt) is a separate frontend + IPC task tracked as a
follow-up.
