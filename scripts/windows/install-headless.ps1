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
    [UInt32] $DdcCode = 0x60,
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

function Quote-TaskArgument {
    param([Parameter(Mandatory = $true)][string] $Value)
    if ($Value -match '[\s"]') {
        '"' + ($Value -replace '"', '\"') + '"'
    } else {
        $Value
    }
}

function Escape-VbsString {
    param([Parameter(Mandatory = $true)][string] $Value)
    $Value.Replace('"', '""')
}

function Write-HiddenLauncher {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $ExePath,
        [Parameter(Mandatory = $true)][string] $LogPath
    )

    $vbsExe = Escape-VbsString $ExePath
    $vbsLog = Escape-VbsString $LogPath
    @(
        "Option Explicit",
        "Dim shell, exe, log, cmd",
        "Set shell = CreateObject(""WScript.Shell"")",
        ('exe = "{0}"' -f $vbsExe),
        ('log = "{0}"' -f $vbsLog),
        'cmd = Chr(34) & exe & Chr(34) & " --log-file " & Chr(34) & log & Chr(34) & " --log-level info run"',
        "WScript.Quit shell.Run(cmd, 0, True)"
    ) | Set-Content -LiteralPath $Path -Encoding ASCII
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

    if (-not $NoTask) {
        $ExistingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        if ($null -ne $ExistingTask) {
            Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 1
        }
    }
    Get-Process -Name "lan-mouse" -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                $_.Path -eq $ExeDest
            } catch {
                $false
            }
        } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    $StopDeadline = (Get-Date).AddSeconds(10)
    do {
        $InstalledProcesses = @(Get-Process -Name "lan-mouse" -ErrorAction SilentlyContinue |
            Where-Object {
                try {
                    $_.Path -eq $ExeDest
                } catch {
                    $false
                }
            })
        if ($InstalledProcesses.Count -eq 0) {
            break
        }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $StopDeadline)
    if ($InstalledProcesses.Count -ne 0) {
        $ProcessIds = ($InstalledProcesses | ForEach-Object { $_.Id }) -join ", "
        throw "Installed lan-mouse.exe is still running and blocks reinstall. PIDs: $ProcessIds"
    }

    Copy-Item -LiteralPath $ExePath -Destination $ExeDest -Force

    $HasMonitorSwitch = -not [string]::IsNullOrWhiteSpace($MonitorSelector) -and
        $MacInput -ne 0 -and
        $WindowsInput -ne 0
    $UseMonitorHooks = $HasMonitorSwitch -and
        -not [string]::IsNullOrWhiteSpace($ControlMyMonitorPath)
    $UseNativeDdc = $HasMonitorSwitch -and -not $UseMonitorHooks

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
    } else {
        Remove-Item -LiteralPath (Join-Path $InstallDir "hook-enter.bat") -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $InstallDir "hook-leave.bat") -Force -ErrorAction SilentlyContinue
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
    if ($UseNativeDdc) {
        $PairArgs += @("--ddc-monitor", $MonitorSelector)
        $PairArgs += @("--ddc-code", ([string] $DdcCode))
        $PairArgs += @("--ddc-enter-input", ([string] $MacInput))
        $PairArgs += @("--ddc-leave-input", ([string] $WindowsInput))
    }

    & $ExeDest @PairArgs
    if ($LASTEXITCODE -ne 0) {
        throw "lan-mouse pair failed with exit code $LASTEXITCODE"
    }

    Remove-Item -LiteralPath $DaemonBat -Force -ErrorAction SilentlyContinue

    $WScriptPath = Join-Path $env:SystemRoot "System32\wscript.exe"
    Require-File $WScriptPath
    Write-HiddenLauncher -Path $LauncherVbs -ExePath $ExeDest -LogPath $LogPath

    if (-not $NoTask) {
        $Action = New-ScheduledTaskAction -Execute $WScriptPath -Argument (Quote-TaskArgument $LauncherVbs)
        $Trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
        $Settings = New-ScheduledTaskSettingsSet `
            -AllowStartIfOnBatteries `
            -DontStopIfGoingOnBatteries `
            -Hidden `
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
        Start-Process -FilePath $WScriptPath -ArgumentList (Quote-TaskArgument $LauncherVbs) -WindowStyle Hidden
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
