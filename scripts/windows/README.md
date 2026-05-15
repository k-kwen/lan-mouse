# Windows headless package scripts

These scripts install the headless Rust runtime for the current Windows user.
They are intended to be bundled with `lan-mouse.exe` in
`lan-mouse-windows-headless-x86_64.zip`.

The installer delegates peer setup to `lan-mouse.exe pair`, so the config is
written by the Rust binary, not by hand-built PowerShell TOML.

## Install

Run PowerShell from the extracted package directory:

```powershell
Set-ExecutionPolicy -Scope Process Bypass -Force
.\install-headless.ps1 `
  -PeerFingerprint "b9:2c:..." `
  -Position left `
  -MacHostname "Kwen-PA-serverui-Macmini.local"
```

For native Rust DDC monitor switching:

```powershell
.\install-headless.ps1 `
  -PeerFingerprint "b9:2c:..." `
  -Position left `
  -MacHostname "Kwen-PA-serverui-Macmini.local" `
  -MonitorSelector "PHLC277" `
  -MacInput 17 `
  -WindowsInput 18
```

`DdcCode` defaults to `0x60`, the VCP input-source code.

If native DDC does not work for a particular monitor, pass
`ControlMyMonitorPath` to keep the legacy hook fallback:

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

You can inspect visible peers before installing:

```powershell
.\lan-mouse.exe discover --json
```

You can test native DDC without moving the cursor across screens:

```powershell
.\lan-mouse.exe test-ddc --monitor PHLC277 --value 18
```

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
