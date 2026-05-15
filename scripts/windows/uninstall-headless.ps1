[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $InstallDir = "$env:LOCALAPPDATA\Programs\LanMouse",
    [string] $ConfigDir = "$env:LOCALAPPDATA\lan-mouse",
    [string] $TaskName = "LanMouse",
    [switch] $RemoveConfig
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$InstallDir = [Environment]::ExpandEnvironmentVariables($InstallDir)
$ConfigDir = [Environment]::ExpandEnvironmentVariables($ConfigDir)

if ($PSCmdlet.ShouldProcess($TaskName, "Unregister scheduled task")) {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($null -ne $task) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    }
}

if (Test-Path -LiteralPath $InstallDir) {
    $resolvedInstall = (Resolve-Path -LiteralPath $InstallDir).Path
    Get-Process -Name "lan-mouse" -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                $_.Path -like "$resolvedInstall*"
            } catch {
                $false
            }
        } |
        Stop-Process -Force -ErrorAction SilentlyContinue

    if ($PSCmdlet.ShouldProcess($InstallDir, "Remove install directory")) {
        Remove-Item -LiteralPath $InstallDir -Recurse -Force
    }
}

if ($RemoveConfig -and (Test-Path -LiteralPath $ConfigDir)) {
    if ($PSCmdlet.ShouldProcess($ConfigDir, "Remove config directory")) {
        Remove-Item -LiteralPath $ConfigDir -Recurse -Force
    }
}

Write-Host "Uninstalled lan-mouse headless runtime"
