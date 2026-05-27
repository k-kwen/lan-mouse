# Mac Work Plan - Dynamic IP Recovery

- Date: 2026-05-27
- Host: macOS `Kwen-PA-serverui-Macmini`
- Peer: Windows `sangwha-KWEN`
- Prerequisite: Windows work is complete and the same commit is pushed to GitHub.

## Decision

Mac must run the same commit as Windows. Windows-only changes do not fix Mac -> Windows handoff because Mac is the dialer when the cursor crosses from Mac to Windows.

Keep `ips = []`. Peer trust is the Windows DTLS certificate fingerprint, not its current IP address.

Tailscale is diagnostic-only. Do not put `100.x.x.x` in config for this plan.

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
tail -n 120 "$HOME/Library/Logs/lan-mouse/daemon.log"
```

Expected:

- Discovery shows `sangwha-KWEN` / `sangwha-KWEN.local`.
- The advertised fingerprint equals `7c:af:...:59`.
- `Cmd + right edge` produces `client 0 acknowledged the connection!` or `client (0) connected`.
- Windows -> Mac still works from the Windows left edge.

## Failure Branches

If discovery only shows the Mac itself, Windows and Mac are not in the same mDNS/LAN domain. Check the Wi-Fi SSID/VLAN/subnet on both sides. The code cannot route across an isolated network boundary by itself.

If discovery works but DTLS times out, check Windows UDP 4242 and firewall:

```powershell
Get-NetUDPEndpoint -LocalPort 4242
Get-NetFirewallRule -DisplayName "*lan-mouse*" |
  Select-Object DisplayName,Enabled,Direction,Action,Profile
```

If fingerprint mismatch appears, re-read Windows `lan-mouse.pem` and update Mac `peer_fingerprint` plus `[authorized_fingerprints]`.
