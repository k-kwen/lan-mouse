# Windows installer and pairing plan

## Goal

Ship a lightweight Windows package that installs the Rust headless runtime and
creates a dynamic-IP-safe Mac peer configuration. The operator should not pin
an IP address. A Mac is identified by its DTLS certificate fingerprint, and
addresses remain volatile transport candidates learned from mDNS, DNS, and the
last successful connection cache.

The first package target is:

```text
lan-mouse-windows-headless-x86_64.zip
```

This intentionally avoids the GTK bundle and only ships:

- `lan-mouse.exe` built with `--no-default-features`
- per-user PowerShell install/uninstall scripts
- the Windows script README

## Installer UX target

1. The Mac build runs first and exposes its "Mac key", which is the peer
   certificate fingerprint.
2. The Windows installer discovers `_lan-mouse._udp.local` records and shows
   candidate Macs. If the discovered TXT `fp` value matches the Mac key, the
   installer can trust that peer without storing any IP address.
3. The user provides:
   - screen position: `left`, `right`, `top`, or `bottom`
   - Mac key: DTLS certificate fingerprint
   - optional monitor input switch settings
4. The installer writes `%LOCALAPPDATA%\lan-mouse\config.toml` with:
   - `peer_fingerprint = "..."`
   - `ips = []`
   - `mdns_discovery = true`
   - an `authorized_fingerprints` entry for the Mac
5. The installer registers a per-user scheduled task and starts the daemon.
6. The installer performs a smoke test with `lan-mouse.exe cli list`.

## Current package scaffold

`scripts/windows/install-headless.ps1` is the current per-user installer. It is
designed to be bundled beside `lan-mouse.exe` and can already create the
dynamic-IP-safe config when the Mac key is supplied.

Example:

```powershell
.\install-headless.ps1 `
  -PeerFingerprint "b9:2c:..." `
  -Position left `
  -MacHostname "Kwen-PA-serverui-Macmini.local" `
  -ControlMyMonitorPath "C:\Tools\ControlMyMonitor.exe" `
  -MonitorSelector "PHLC277" `
  -MacInput 17 `
  -WindowsInput 18
```

The hostname is a label and DNS fallback, not the trust anchor. The
fingerprint is the trust anchor. The generated client keeps `ips = []`.

## Remaining Rust work for full auto-pairing

### W1: `lan-mouse discover`

Add a top-level command that browses `_lan-mouse._udp.local` for a short window
and prints JSON:

```json
[
  {
    "instance": "Kwen-PA-serverui-Macmini",
    "hostname": "Kwen-PA-serverui-Macmini.local",
    "port": 4242,
    "fingerprint": "b9:2c:...",
    "addresses": ["192.168.10.155", "fe80::..."]
  }
]
```

This command must not require the daemon to be running. It should reuse the
same mDNS service type and TXT keys as `src/discovery.rs`.

### W2: `lan-mouse pair`

Add a top-level command that writes config directly:

```powershell
lan-mouse.exe pair --mac-key "b9:2c:..." --position left --hostname optional
```

Behavior:

- browse mDNS and prefer a discovered peer whose TXT `fp` matches `--mac-key`
- write a client with `peer_fingerprint`, `ips = []`, and `activate_on_startup`
- add the Mac fingerprint under `[authorized_fingerprints]`
- run a connection smoke test when the daemon is already active

### W3: monitor switching without external hooks

The current working Windows setup uses `ControlMyMonitor.exe` with selector
`PHLC277`. The native DDC path exists, but it needs a more practical monitor
selector that can match Windows monitor IDs and short IDs. Extend the native
selector before removing hook fallback from the installer.

Required selector inputs:

- zero-based enumeration index
- physical monitor description
- display device name, e.g. `\\.\DISPLAY10`
- short monitor ID, e.g. `PHLC277`

### W4: MSI/MSIX wrapper

After W1-W3, wrap the same headless package in an MSI or MSIX. Keep the
PowerShell installer as the transparent, debuggable backend first; the wrapper
should only provide UI and elevation-free per-user install ergonomics.

## Verification loop

Each package milestone should run this loop:

1. Build: `cargo build --release --no-default-features`
2. Unit tests: `cargo test --no-default-features`
3. Package smoke: install from a clean temporary directory
4. Runtime smoke: scheduled task starts, `cli list` shows one active Mac peer
5. Discovery smoke: change/relearn Mac IP without editing `config.toml`
6. Monitor smoke: enter and leave switch the monitor input exactly once

## Non-goals

- Do not require a fixed IP address or DHCP reservation.
- Do not require the full GTK Windows bundle for the headless package.
- Do not make IP address the peer identity. The peer identity is the
  fingerprint.
