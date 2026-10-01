# The sandbox cannot provision a workspace it does not own

Found on 2026-10-01 while verifying the page-debugging plugin end to end: an
agent in a container called `box_screenshot`, the screenshot was captured, and
the image never reached the model.

    Error: SetNamedSecurityInfoW failed (Win32 5): grantWrite(D:\zs\V98Pr)

`grantWrite` is the sandbox granting its confined token write access to a
writable directory (`sandbox-windows-acl/src/index.ts`, over `writableDirs` and
the session temp dir). It failed because it could not modify the directory's
DACL.

## What the diagnostic showed

`diagnose-windows-sandbox-acl.ps1 -Path 'D:\zs\V98Pr' -AllowRoot 'D:\zs'`:

    D:\zs\V98Pr   owner=S-1-5-32-544 (Administrators)  writeDac=false  writeOwner=false
    D:\zs         owner=S-1-5-21-...-1001 (current user) writeDac=true
    D:\           writeDac=false

The directory is owned by Administrators and the signed-in user holds neither
right, so the script refuses rather than guess:

    status: refused -- Effective WRITE_DAC is absent; adding the grant requires that right.

Unconfined execution is not elevation, so this cannot be cleared from a normal
process. The container's own sandbox identity is in the same position: it needs
to add an ACE to a directory whose DACL it cannot write.

## Two things worth knowing

`icacls` was not on `PATH` in the diagnostic environment, so the script's
observations came back incomplete and it refused the first run as INCOMPLETE
rather than repairing on partial evidence. With `C:\Windows\System32` on `PATH`
the observations are complete and the refusal is the real answer. A refused
repair here means a missing right, not a broken tool.

The path that matters is the **workspace**, which lives outside the container
tree. Repairing the container's own `D:\ddd\...\workspace` changes nothing:
the sandbox never grants there.

## Remedy

One of these, run by an administrator:

1. Take ownership of the workspace and hand it to the signed-in user:

       takeown /F "D:\zs\V98Pr" /R /D Y
       icacls "D:\zs\V98Pr" /grant "%USERNAME%":(OI)(CI)F /T

2. Or point the session at a workspace inside the container tree, which the
   signed-in user already owns.

Until then the plugin is verified as far as the tool call and the screenshot
RPC; only the attachment hand-off is unproven.
