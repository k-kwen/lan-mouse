# macOS Headless Install Package

These scripts install the command-line Lan Mouse daemon for the current macOS
user. They do not require root.

The installer:

- copies `lan-mouse` to `~/Tools/lan-mouse/lan-mouse` by default
- signs the copied binary with `Lan-mouse Dev Cert` when available
- falls back to ad-hoc signing when the certificate is missing
- writes `~/Library/LaunchAgents/com.<user>.lan-mouse.plist`
- writes daemon logs to `~/Library/Logs/lan-mouse/daemon.log`
- starts the daemon automatically
- opens the macOS Privacy panes and prints the permission steps

The installer preserves `~/.config/lan-mouse/config.toml`; it does not create or
overwrite pairing configuration.

## Install From A Package

Extract the package and run:

```sh
./install-headless.sh
```

Useful options:

```sh
./install-headless.sh --source ./lan-mouse
./install-headless.sh --install-dir "$HOME/Tools/lan-mouse"
./install-headless.sh --label "com.kwenpa.lan-mouse"
./install-headless.sh --log-level debug
./install-headless.sh --no-open-privacy
```

After install, grant permissions in System Settings:

- Privacy & Security -> Accessibility -> add/toggle the installed `lan-mouse`
- Privacy & Security -> Input Monitoring -> add/toggle the installed `lan-mouse`

Then restart the daemon:

```sh
launchctl kickstart -k "gui/$(id -u)/com.$USER.lan-mouse"
```

## Logs

The LaunchAgent sets `LAN_MOUSE_LOG_FILE` and `LAN_MOUSE_LOG_LEVEL`, so daemon
logs are written directly by the process:

```sh
tail -f "$HOME/Library/Logs/lan-mouse/daemon.log"
```

The stdout/stderr files remain available for startup failures before logging is
initialized.

## Uninstall

```sh
./uninstall-headless.sh
```

By default, uninstall removes the LaunchAgent and installed binary, but keeps
config and logs. To remove those too:

```sh
./uninstall-headless.sh --remove-config --remove-logs
```

Privacy grants are not reset automatically. Remove the app manually from System
Settings if you want to clear them.

## Build A Package

From the repository root:

```sh
scripts/macos/package-headless.sh
```

This builds `target/release/lan-mouse` with `--no-default-features` and writes a
tarball under `target/dist/`.
