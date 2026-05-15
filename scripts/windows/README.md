# Windows headless package scripts

These scripts install the headless Rust runtime for the current Windows user.
They are intended to be bundled with `lan-mouse.exe` in
`lan-mouse-windows-headless-x86_64.zip`.

## Install

Run PowerShell from the extracted package directory:

```powershell
Set-ExecutionPolicy -Scope Process Bypass -Force
.\install-headless.ps1 `
  -PeerFingerprint "b9:2c:..." `
  -Position left `
  -MacHostname "Kwen-PA-serverui-Macmini.local"
```

For the current Philips monitor switching setup:

```powershell
.\install-headless.ps1 `
  -PeerFingerprint "b9:2c:..." `
  -Position left `
  -MacHostname "Kwen-PA-serverui-Macmini.local" `
  -ControlMyMonitorPath "C:\Users\user\Tools\ControlMyMonitor\ControlMyMonitor.exe" `
  -MonitorSelector "PHLC277" `
  -MacInput 17 `
  -WindowsInput 18
```

`PeerFingerprint` is the Mac key. The generated config intentionally keeps
`ips = []`; dynamic addresses are learned at runtime.

## Uninstall

```powershell
.\uninstall-headless.ps1
```

Add `-RemoveConfig` to also remove `%LOCALAPPDATA%\lan-mouse`.

## Generated files

- `%LOCALAPPDATA%\Programs\LanMouse\lan-mouse.exe`
- `%LOCALAPPDATA%\Programs\LanMouse\daemon.bat`
- `%LOCALAPPDATA%\Programs\LanMouse\lan-mouse-hidden.vbs`
- `%LOCALAPPDATA%\lan-mouse\config.toml`
- `%LOCALAPPDATA%\lan-mouse\daemon.log`
- Scheduled task: `LanMouse`
