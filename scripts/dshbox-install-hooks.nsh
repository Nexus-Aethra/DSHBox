; DSH Box NSIS installer hooks.
;
; Tauri generates the base installer.nsi and !include's this file at the
; hook points documented in its NsisConfig. We use the hooks to put
; `dshbox` on the user's PATH so it resolves in every shell the instant
; the installer finishes.
;
; Deliberate design choices:
;   - We touch only HKCU\Environment\Path (user-level). A per-machine
;     install may not have rights to write the machine-level PATH.
;   - Add: blindly append $INSTDIR to PATH on every install. Duplicate
;     entries are cosmetically messy but functionally harmless, and
;     deduping PATH in pure NSIS without plugins is fragile.
;   - Uninstall: we do NOT strip the entry from PATH. Cleaning the user
;     PATH is a one-line PowerShell operation that's well-documented in
;     sysdm.cpl, and trying to do it correctly from within the
;     uninstaller would need nsExec or a self-hosted helper. Leave PATH
;     intact — the user can clean it up with one command if they want.
;   - WM_SETTINGCHANGE is broadcast after the change so every newly
;     spawned process (PowerShell, cmd.exe, Explorer, shortcut targets)
;     sees the updated PATH without a reboot.
;
; The hook macros are all we need. Everything else is NSIS
; boilerplate that Tauri's own template already provides.

!define HWND_BROADCAST_CONST 0xFFFF
!define WM_SETTINGCHANGE_CONST 0x001A

!macro NSIS_HOOK_POSTINSTALL
  ; Pull the current user PATH. If the value doesn't exist yet (first
  ; install ever on a fresh machine), we start from an empty string.
  ReadRegStr $0 HKCU "Environment" "Path"

  ; Strip any trailing semicolon from the current value so appending
  ; produces a clean "A;B;D:\dshbox" instead of "A;B;;D:\dshbox".
  StrLen $1 $0
  ${If} $1 > 0
    StrCpy $2 $0 -1 $1
    ${If} $2 == ";"
      StrCpy $0 $0 -1
    ${EndIf}
  ${EndIf}

  ; Strip a trailing backslash from the install dir so we don't create
  ; "A;B;D:\dshbox\" entries that fail as command lookup targets.
  StrLen $1 "$INSTDIR"
  ${If} $1 > 0
    StrCpy $2 "$INSTDIR" -1 $1
    ${If} $2 == "\"
      StrCpy $INSTDIR "$INSTDIR" -1
    ${EndIf}
  ${EndIf}

  ; Append.
  ${If} $0 == ""
    StrCpy $0 "$INSTDIR"
  ${Else}
    StrCpy $0 "$0;$INSTDIR"
  ${EndIf}

  ; Persist the updated value and broadcast so every new terminal,
  ; PowerShell, cmd.exe, and shortcut-target sees `dshbox` immediately.
  WriteRegStr HKCU "Environment" "Path" $0
  SendMessage ${HWND_BROADCAST_CONST} ${WM_SETTINGCHANGE_CONST} 0 "Environment"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; We deliberately do not strip $INSTDIR from PATH. See design
  ; notes at the top of this file.
!macroend
