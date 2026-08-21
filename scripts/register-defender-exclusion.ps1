<#
DSH Box Defender exclusion registration (manual fallback).

The desktop app registers these exclusions automatically: on first run it
asks once via UAC (src-tauri/src/desktop/app/defender.rs) so Windows
Defender real-time scan does not race pnpm install inside DSH Box's
working directories (the EBUSY/UNKNOWN pattern that breaks container
prepare). Run this script by hand only when the automatic prompt was
dismissed and you want to register later without reinstalling:

  powershell -ExecutionPolicy Bypass -File scripts/register-defender-exclusion.ps1 `
      -InstallDir 'D:\dshbox' -RuntimeDir 'D:\ddd'

Idempotent: Add-MpPreference accepts duplicate paths without error. The
script also tolerates hosts without Defender (Server Core, custom
anti-virus) — it logs a warning and exits 0.
#>

[CmdletBinding()]
param(
    [string]$InstallDir = "$env:ProgramFiles\DSH Box",
    [string]$RuntimeDir = "$env:USERPROFILE\.dsh-box",
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

function Remove-DefenderExclusion([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path)) {
        Write-Host "skip: $Label path does not exist ($Path)"
        return
    }
    try {
        Remove-MpPreference -ExclusionPath $Path -ErrorAction Stop
        Write-Host "ok: $Label exclusion removed for $Path"
    } catch {
        if ($_.Exception.Message -match 'no such interface|0x80004002') {
            Write-Host "skip: $Label host has no Defender interface; nothing to remove"
        } else {
            Write-Warning "could not remove $Label exclusion for $Path : $($_.Exception.Message)"
        }
    }
}

function Add-DefenderExclusion([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path)) {
        Write-Host "skip: $Label path does not exist ($Path)"
        return
    }
    try {
        Add-MpPreference -ExclusionPath $Path -ErrorAction Stop
        Write-Host "ok: $Label exclusion added for $Path"
    } catch {
        if ($_.Exception.Message -match 'no such interface|0x80004002') {
            Write-Host "skip: $Label host has no Defender interface; nothing to register"
        } else {
            Write-Warning "could not add $Label exclusion for $Path : $($_.Exception.Message)"
        }
    }
}

if ($Uninstall) {
    Remove-DefenderExclusion -Path $InstallDir -Label 'install-dir'
    Remove-DefenderExclusion -Path $RuntimeDir -Label 'runtime-dir'
    exit 0
}

Add-DefenderExclusion -Path $InstallDir -Label 'install-dir'
Add-DefenderExclusion -Path $RuntimeDir -Label 'runtime-dir'
exit 0
