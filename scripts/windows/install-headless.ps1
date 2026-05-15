[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $ExePath = "",
    [string] $InstallDir = "$env:LOCALAPPDATA\Programs\LanMouse",
    [string] $ConfigDir = "$env:LOCALAPPDATA\lan-mouse",
    [string] $TaskName = "LanMouse",
    [string] $MacHostname = "",
    [Alias("MacKey")]
    [string] $PeerFingerprint = "",
    [ValidateSet("left", "right", "top", "bottom")]
    [string] $Position = "left",
    [UInt16] $Port = 4242,
    [string] $PeerLabel = "mac",
    [string] $ControlMyMonitorPath = "",
    [string] $MonitorSelector = "",
    [UInt32] $MacInput = 0,
    [UInt32] $WindowsInput = 0,
    [switch] $NoStart,
    [switch] $NoTask
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-InstallPath {
    param([Parameter(Mandatory = $true)][string] $Path)
    [Environment]::ExpandEnvironmentVariables($Path)
}

function Escape-BatchString {
    param([Parameter(Mandatory = $true)][string] $Value)
    $Value.Replace("%", "%%")
}

function Require-File {
    param([Parameter(Mandatory = $true)][string] $Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Required file not found: $Path"
    }
}

if ([string]::IsNullOrWhiteSpace($PeerFingerprint)) {
    throw "PeerFingerprint is required. Use the Mac certificate fingerprint as the Mac key."
}

$PeerFingerprint = $PeerFingerprint.Trim().ToLowerInvariant()
$InstallDir = Resolve-InstallPath $InstallDir
$ConfigDir = Resolve-InstallPath $ConfigDir

if ([string]::IsNullOrWhiteSpace($ExePath)) {
    $ExePath = Join-Path $PSScriptRoot "lan-mouse.exe"
}
$ExePath = Resolve-InstallPath $ExePath
Require-File $ExePath

$ExeDest = Join-Path $InstallDir "lan-mouse.exe"
$DaemonBat = Join-Path $InstallDir "daemon.bat"
$LauncherVbs = Join-Path $InstallDir "lan-mouse-hidden.vbs"
$ConfigPath = Join-Path $ConfigDir "config.toml"
$LogPath = Join-Path $ConfigDir "daemon.log"

if ($PSCmdlet.ShouldProcess($InstallDir, "Install lan-mouse headless runtime")) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

    Copy-Item -LiteralPath $ExePath -Destination $ExeDest -Force

    $UseMonitorHooks = -not [string]::IsNullOrWhiteSpace($ControlMyMonitorPath) -and
        -not [string]::IsNullOrWhiteSpace($MonitorSelector) -and
        $MacInput -ne 0 -and
        $WindowsInput -ne 0

    if ($UseMonitorHooks) {
        $ControlMyMonitorPath = Resolve-InstallPath $ControlMyMonitorPath
        Require-File $ControlMyMonitorPath

        $EnterHook = Join-Path $InstallDir "hook-enter.bat"
        $LeaveHook = Join-Path $InstallDir "hook-leave.bat"
        $batMonitor = Escape-BatchString $MonitorSelector
        $batControl = Escape-BatchString $ControlMyMonitorPath

        @(
            "@echo off",
            ('"{0}" /SetValueIfNeeded "{1}" 60 {2}' -f $batControl, $batMonitor, $MacInput)
        ) | Set-Content -LiteralPath $EnterHook -Encoding ASCII

        @(
            "@echo off",
            ('"{0}" /SetValueIfNeeded "{1}" 60 {2}' -f $batControl, $batMonitor, $WindowsInput)
        ) | Set-Content -LiteralPath $LeaveHook -Encoding ASCII
    }

    $PairArgs = @(
        "--config", $ConfigPath,
        "pair",
        "--mac-key", $PeerFingerprint,
        "--position", $Position,
        "--label", $PeerLabel,
        "--port", ([string] $Port),
        "--discover-timeout-ms", "2500"
    )
    if (-not [string]::IsNullOrWhiteSpace($MacHostname)) {
        $PairArgs += @("--hostname", $MacHostname.Trim())
    }
    if ($UseMonitorHooks) {
        $PairArgs += @("--enter-hook", $EnterHook)
        $PairArgs += @("--leave-hook", $LeaveHook)
    }

    & $ExeDest @PairArgs
    if ($LASTEXITCODE -ne 0) {
        throw "lan-mouse pair failed with exit code $LASTEXITCODE"
    }

    @(
        "@echo off",
        "setlocal",
        ('"{0}" --log-file "{1}" --log-level info run' -f (Escape-BatchString $ExeDest), (Escape-BatchString $LogPath))
    ) | Set-Content -LiteralPath $DaemonBat -Encoding ASCII

    @(
        'Set shell = CreateObject("WScript.Shell")',
        'If WScript.Arguments.Count = 0 Then WScript.Quit 1',
        'shell.Run """" & WScript.Arguments(0) & """", 0, False'
    ) | Set-Content -LiteralPath $LauncherVbs -Encoding ASCII

    if (-not $NoTask) {
        $ActionArgs = ('"{0}" "{1}"' -f $LauncherVbs, $DaemonBat)
        $Action = New-ScheduledTaskAction -Execute "$env:WINDIR\System32\wscript.exe" -Argument $ActionArgs
        $Trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
        $Settings = New-ScheduledTaskSettingsSet `
            -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries `
            -StartWhenAvailable `
            -RestartCount 3 `
            -RestartInterval (New-TimeSpan -Minutes 1)

        Register-ScheduledTask `
            -TaskName $TaskName `
            -Action $Action `
            -Trigger $Trigger `
            -Settings $Settings `
            -Description "Lan Mouse headless daemon" `
            -Force | Out-Null

        if (-not $NoStart) {
            Start-ScheduledTask -TaskName $TaskName
            Start-Sleep -Seconds 2
        }
    } elseif (-not $NoStart) {
        Start-Process -FilePath $DaemonBat -WindowStyle Hidden
        Start-Sleep -Seconds 2
    }

    Write-Host "Installed lan-mouse to $InstallDir"
    Write-Host "Config written to $ConfigPath"
    Write-Host "Peer fingerprint: $PeerFingerprint"

    if (-not $NoStart) {
        try {
            & $ExeDest cli list
        } catch {
            Write-Warning "Smoke test failed: $($_.Exception.Message)"
        }
    }
}
