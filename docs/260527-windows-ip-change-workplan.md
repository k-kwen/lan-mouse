# Windows Work Plan - Dynamic IP Recovery

- Date: 2026-05-27
- Host: Windows `sangwha-KWEN`
- Peer: macOS `Kwen-PA-serverui-Macmini`
- Goal: keep normal operation on `ips = []`; identify peers by DTLS certificate fingerprint and refresh address candidates automatically when LAN IPs change.

## Decision

This is a shared Windows/macOS source change, not a Windows-only fix. Build and deploy the same commit on both machines.

Tailscale is diagnostic-only in this plan. Do not put `100.x.x.x` addresses into lan-mouse config unless deliberately reverting to the old Tailscale transport experiment.

## Fingerprints

Windows:

```text
7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59
```

Mac:

```text
b9:2c:9f:78:e0:ec:e8:e7:b6:59:b3:67:11:03:52:e4:30:cb:a7:18:a8:01:fb:33:02:be:8a:66:ee:13:2d:24
```

Windows target config:

```toml
port = 4242
mdns_discovery = true

[[clients]]
hostname = "Kwen-PA-serverui-Macmini.local"
peer_fingerprint = "b9:2c:9f:78:e0:ec:e8:e7:b6:59:b3:67:11:03:52:e4:30:cb:a7:18:a8:01:fb:33:02:be:8a:66:ee:13:2d:24"
ips = []
position = "left"
activate_on_startup = true

[authorized_fingerprints]
"b9:2c:9f:78:e0:ec:e8:e7:b6:59:b3:67:11:03:52:e4:30:cb:a7:18:a8:01:fb:33:02:be:8a:66:ee:13:2d:24" = "mac"
```

## What This Commit Changes

- Active client hostnames are resolved every 30 seconds.
- If mDNS browse is delayed or lost, the OS resolver can still refresh `.local` / DNS IP candidates.
- mDNS now advertises every usable LAN IPv4 address on the host, not only the default-route IP.
- Tailscale/CGNAT `100.64.0.0/10`, loopback, multicast, and link-local addresses are excluded from dynamic discovery caches.
- A lightweight fingerprint fallback probe listens on UDP `4243`.
- The fallback probe only sends a small LAN broadcast when an active peer has no active connection and no static/DNS candidates; stale mDNS or last-success hints do not block refresh.
- Hostname refresh only runs for active clients that do not currently have an active DTLS address.
- If a peer has no usable address candidates, no DTLS connect task is spawned on edge crossing.
- Capture is not armed while the peer is unresolved or in retry backoff, so the daemon stays light when the peer is offline.
- Repeated identical DNS failures are logged at debug level after the first warning.
- `cli list` prints `alive`, `resolving`, `active_addr`, and `peer_commit`.

## Windows Build

```powershell
cd "C:\Users\user\OneDrive - 주식회사 상화\바탕 화면\GWS\LAN MOUSE\source\lan-mouse"
cargo fmt
cargo test -p lan-mouse --no-default-features
cargo build --release --no-default-features
```

## Install On Windows

```powershell
$tool = "$env:USERPROFILE\Tools\lan-mouse"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"

Stop-Process -Name lan-mouse -Force -ErrorAction SilentlyContinue
Copy-Item "$tool\lan-mouse.exe" "$tool\lan-mouse.exe.bak-$stamp" -Force
Copy-Item ".\target\release\lan-mouse.exe" "$tool\lan-mouse.exe" -Force

Start-Process "$tool\lan-mouse.exe" `
  -ArgumentList @("--log-file", "$env:LOCALAPPDATA\lan-mouse\daemon.log", "--log-level", "info", "daemon") `
  -WorkingDirectory $tool `
  -WindowStyle Hidden
```

## Verify Windows

```powershell
Get-CimInstance Win32_Process -Filter "Name='lan-mouse.exe'" |
  Select-Object ProcessId,CommandLine,ExecutablePath

Get-NetUDPEndpoint -LocalPort 4242 |
  Select-Object LocalAddress,LocalPort,OwningProcess

Get-NetUDPEndpoint -LocalPort 4243 |
  Select-Object LocalAddress,LocalPort,OwningProcess

& "$env:USERPROFILE\Tools\lan-mouse\lan-mouse.exe" --version
& "$env:USERPROFILE\Tools\lan-mouse\lan-mouse.exe" cli list
& "$env:USERPROFILE\Tools\lan-mouse\lan-mouse.exe" discover --json --timeout-ms 10000
```

Expected:

- UDP 4242 listens on the current Windows LAN IP.
- UDP 4243 listens for the lightweight fingerprint fallback probe.
- `cli list` includes `peer_fingerprint`, `alive`, `resolving`, and optionally `active_addr`.
- If Mac is reachable through the same mDNS/LAN domain, `discover` shows the Mac fingerprint and may list multiple LAN addresses.
- If Mac is not reachable, edge crossing logs at most occasional `capture not armed` messages and does not create repeated DTLS work.
- If mDNS is blocked but LAN broadcast works, the fallback probe can still cache the Mac IP by fingerprint.

If Windows Firewall does not already have a program rule for `lan-mouse.exe`, allow the three UDP paths:

```powershell
New-NetFirewallRule -DisplayName "lan-mouse UDP 4242 DTLS" `
  -Direction Inbound -Protocol UDP -LocalPort 4242 `
  -Action Allow -Profile Any

New-NetFirewallRule -DisplayName "lan-mouse UDP 4243 fallback discovery" `
  -Direction Inbound -Protocol UDP -LocalPort 4243 `
  -Action Allow -Profile Any

New-NetFirewallRule -DisplayName "lan-mouse UDP 5353 mDNS" `
  -Direction Inbound -Protocol UDP -LocalPort 5353 `
  -Action Allow -Profile Any
```

## Push For Mac

Only after checking unrelated local changes:

```powershell
git status --short
git diff --check
git add src/discovery.rs src/service.rs docs/260527-windows-ip-change-workplan.md docs/260527-mac-ip-change-workplan.md
git commit -m "Improve dynamic LAN peer discovery"
git push origin kwen-mdns-hooks
```

If `scripts/windows/README.md` or `scripts/windows/install-headless.ps1` are already modified, review whether they belong in this commit or a separate one.

## Hard Limit

If Windows and Mac are on different VLANs/subnets and neither mDNS multicast nor same-LAN broadcast can cross that boundary, lan-mouse cannot discover across that boundary with LAN discovery alone. Fix the network path or intentionally choose another transport; do not hide that by adding stale static IPs.
