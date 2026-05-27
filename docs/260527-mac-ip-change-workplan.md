# Mac Work Plan - Dynamic IP Recovery

- Date: 2026-05-27
- Host: macOS `Kwen-PA-serverui-Macmini`
- Peer: Windows `sangwha-KWEN`
- Prerequisite: Windows work is complete and the same commit is pushed to GitHub.

## Decision

Mac must run the same commit as Windows. Windows-only changes do not fix Mac -> Windows handoff because Mac is the dialer when the cursor crosses from Mac to Windows.

Keep `ips = []`. Peer trust is the Windows DTLS certificate fingerprint, not its current IP address.

Tailscale is diagnostic-only. Do not put `100.x.x.x` in config for this plan.

When Windows is not discoverable, the Mac daemon must keep running but stay light:

- Keep UDP 4242 listening and mDNS advertisement alive.
- Advertise every usable Mac LAN IPv4 address through mDNS, not only the default-route address.
- Exclude Tailscale/CGNAT `100.64.0.0/10`, loopback, multicast, and link-local addresses from dynamic discovery caches.
- Keep UDP 4243 listening for the lightweight fingerprint fallback probe.
- Send the fallback probe only when Windows has no active connection and no static/DNS candidates; stale mDNS or last-success hints do not block refresh.
- Do not arm capture when the Windows peer has no address candidates.
- Do not spawn DTLS connect tasks while unresolved or in retry backoff.
- Retry through mDNS browse, fallback probe, last-success, and low-rate hostname refresh once candidates appear.
- Repeated identical DNS failures should not fill the log.

## Fingerprints

Windows:

```text
7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59
```

Mac:

```text
b9:2c:9f:78:e0:ec:e8:e7:b6:59:b3:67:11:03:52:e4:30:cb:a7:18:a8:01:fb:33:02:be:8a:66:ee:13:2d:24
```

Mac target config:

```toml
port = 4242
mdns_discovery = true
release_bind = ["KeyLeftCtrl", "KeyLeftShift", "KeyLeftMeta", "KeyLeftAlt"]

[[clients]]
hostname = "sangwha-KWEN.local"
peer_fingerprint = "7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59"
ips = []
position = "right"
activate_on_startup = true

[authorized_fingerprints]
"7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59" = "windows"
```

## Baseline

```sh
date
hostname
scutil --get LocalHostName || true
ifconfig | egrep '^[a-z0-9]+:|inet '
pgrep -af 'lan-mouse' || true

mkdir -p "$HOME/Tools/lan-mouse-backup"
cp -a "$HOME/.config/lan-mouse" "$HOME/Tools/lan-mouse-backup/config-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true
```

Confirm Mac fingerprint:

```sh
openssl x509 -in "$HOME/.config/lan-mouse/lan-mouse.pem" \
  -noout -fingerprint -sha256 |
  sed 's/SHA256 Fingerprint=//' |
  tr 'A-F' 'a-f'
```

## Fetch Same Commit

```sh
mkdir -p "$HOME/Tools"
cd "$HOME/Tools"

if [ ! -d lan-mouse-src/.git ]; then
  git clone https://github.com/k-kwen/lan-mouse.git lan-mouse-src
fi

cd lan-mouse-src
git fetch origin
git checkout kwen-mdns-hooks
git pull --ff-only origin kwen-mdns-hooks
git log -1 --oneline
```

The last commit must match the Windows build.

## Build

```sh
cd "$HOME/Tools/lan-mouse-src"
cargo fmt --check
cargo test -p lan-mouse --no-default-features
cargo build --release --no-default-features
```

## Install Binary

```sh
mkdir -p "$HOME/Tools/lan-mouse"
pkill -f 'lan-mouse.*daemon' || true
pkill -f 'lan-mouse.*run' || true

if [ -f "$HOME/Tools/lan-mouse/lan-mouse" ]; then
  cp "$HOME/Tools/lan-mouse/lan-mouse" "$HOME/Tools/lan-mouse/lan-mouse.bak-$(date +%Y%m%d-%H%M%S)"
fi

cp "$HOME/Tools/lan-mouse-src/target/release/lan-mouse" "$HOME/Tools/lan-mouse/lan-mouse"
chmod +x "$HOME/Tools/lan-mouse/lan-mouse"
```

## Write Config

```sh
mkdir -p "$HOME/.config/lan-mouse"
cp "$HOME/.config/lan-mouse/config.toml" "$HOME/.config/lan-mouse/config.toml.bak-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true

cat > "$HOME/.config/lan-mouse/config.toml" <<'EOF'
port = 4242
mdns_discovery = true
release_bind = ["KeyLeftCtrl", "KeyLeftShift", "KeyLeftMeta", "KeyLeftAlt"]

[[clients]]
hostname = "sangwha-KWEN.local"
peer_fingerprint = "7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59"
ips = []
position = "right"
activate_on_startup = true

[authorized_fingerprints]
"7c:af:62:e5:16:9b:b4:82:74:cd:fc:e0:20:11:7f:bc:8a:ce:0d:eb:54:ec:f2:cd:79:cf:92:ff:cb:23:34:59" = "windows"
EOF
```

## Permissions And Sleep

Allow the lan-mouse binary/app in:

- Accessibility
- Input Monitoring

Recommended power settings:

```sh
pmset -g
sudo pmset -a sleep 0
sudo pmset -a powernap 0
sudo pmset -a tcpkeepalive 1
sudo pmset -a networkoversleep 1
```

## Start

```sh
mkdir -p "$HOME/Library/Logs/lan-mouse"
"$HOME/Tools/lan-mouse/lan-mouse" \
  --log-file "$HOME/Library/Logs/lan-mouse/daemon.log" \
  --log-level info \
  daemon &
sleep 3
pgrep -af 'lan-mouse'
```

If using LaunchAgent, update the plist to point at `$HOME/Tools/lan-mouse/lan-mouse` with `daemon` or `run`, then restart it:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/de.feschber.LanMouse.plist" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/de.feschber.LanMouse.plist"
```

## Verify

```sh
"$HOME/Tools/lan-mouse/lan-mouse" --version
"$HOME/Tools/lan-mouse/lan-mouse" discover --json --timeout-ms 10000
lsof -nP -iUDP:4242 -iUDP:4243
tail -n 120 "$HOME/Library/Logs/lan-mouse/daemon.log"
```

Expected:

- Discovery shows `sangwha-KWEN` / `sangwha-KWEN.local` if mDNS can cross the LAN path.
- The advertised fingerprint equals `7c:af:...:59`.
- The mDNS address list may include multiple Windows LAN addresses, but not Tailscale `100.x`.
- If mDNS is blocked but LAN broadcast works, the fallback probe can still cache the Windows IP by fingerprint.
- `Cmd + right edge` produces `client 0 acknowledged the connection!` or `client (0) connected`.
- Windows -> Mac still works from the Windows left edge.
- If Windows is not reachable, `Cmd + right edge` should not freeze or wait on repeated connection attempts; the log should show occasional `capture not armed` messages instead.

## Failure Branches

If discovery only shows the Mac itself, mDNS is still not crossing the LAN path. The new fallback probe may still recover if UDP broadcast reaches Windows on the same LAN. If both mDNS and broadcast are blocked, check the Wi-Fi SSID/VLAN/subnet on both sides. The code cannot route across an isolated network boundary by itself.

If discovery works but DTLS times out, check Windows UDP 4242, UDP 4243, UDP 5353, and firewall:

```powershell
Get-NetUDPEndpoint -LocalPort 4242
Get-NetUDPEndpoint -LocalPort 4243
Get-NetFirewallRule -DisplayName "*lan-mouse*" |
  Select-Object DisplayName,Enabled,Direction,Action,Profile
```

If fingerprint mismatch appears, re-read Windows `lan-mouse.pem` and update Mac `peer_fingerprint` plus `[authorized_fingerprints]`.
