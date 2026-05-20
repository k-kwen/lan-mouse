# Windows lock recovery plan

Date: 2026-05-20

## Observed failure

When both the macOS host and the Windows peer are locked, the cursor can appear
to return after unlock while lan-mouse input no longer works reliably. The macOS
side had a recoverability gap: if the CGEvent tap was disabled by user input,
session lock, or timeout pressure, the capture task exited and waited forever
for a manual `enable-capture` request in a headless daemon.

The macOS side now auto-retries recoverable event-tap failures and logs directly
to `~/Library/Logs/lan-mouse/daemon.log`, so future lock/unlock failures should
leave usable evidence without needing stdout/stderr to be connected at launch.
It also debounces the macOS remote `Right Alt` IME toggle path, because rapid
duplicate Right Alt down events can make PriType/ABC input sources flip twice
and leave the visible language popup out of sync with actual text input.

## Current Windows behavior

Windows capture already has partial lock handling in
`input-capture/src/windows/event_thread.rs`:

- It registers for `WM_WTSSESSION_CHANGE`.
- On `WTS_SESSION_LOCK`, it sets `HOST_LOCKED`.
- If Windows is actively capturing another client, it sends
  `CaptureEvent::AutoRelease`.
- While `HOST_LOCKED` is true, barrier crossing is suppressed.

That protects the Windows-as-capturing-host path.

The weaker path is Windows-as-receiving-peer:

- `src/emulation.rs` accepts `ProtoEvent::Enter`, replies `Ack`, sends
  `Bounds`, and marks the peer as entered before checking whether the Windows
  desktop can actually receive input.
- `input-emulation/src/windows.rs` calls `SendInput` and `SetCursorPos`
  directly.
- Windows lock screen / secure desktop can reject or isolate synthetic input.
  Even worse, `send_input_safe` loops until `SendInput` reports success, so a
  locked or unavailable input desktop can become a busy or stuck emulation path.

This can leave the macOS side believing handoff succeeded while Windows cannot
consume the input, especially if both sides are locked and unlock timing races
with an `Enter`/`Ack` exchange.

There is also a Windows-to-macOS IME interaction to keep in mind. The Windows
side sends the Korean keyboard's Hangul/English key as a Right Alt-style event in
the current path, and macOS maps that to direct Text Input Source switching.
If Windows emits both a Hangul semantic event and a Right Alt key event, or emits
duplicate Right Alt down/up pairs around a language switch, macOS can see
multiple toggles for one intended switch. The macOS side now has a short
debounce, but the Windows side should still avoid sending duplicate semantic
toggle events.

## Fix direction

Add lock-state awareness to the Windows emulation side, not only capture:

1. Track session lock state in Windows emulation.
   Use a small Windows message window or shared session-state service to receive
   `WM_WTSSESSION_CHANGE` for `WTS_SESSION_LOCK` and `WTS_SESSION_UNLOCK`.

2. Reject or defer remote entry while locked.
   Before replying `Ack` to `ProtoEvent::Enter`, the listener/service needs to
   know whether the emulation backend is available. If Windows is locked, do not
   mark `Entered`; either reply `Leave` immediately or introduce an explicit
   "unavailable/locked" protocol response.

3. Make `SendInput` bounded.
   Replace the infinite retry loop in `send_input_safe` with a bounded retry or
   immediate error return. Log `GetLastError()` on failure. The caller should
   tear down emulation state and notify the peer rather than spinning.

4. Release on lock.
   If Windows locks while it is receiving remote input, flush pressed keys and
   buttons, destroy the active emulation handle, and send `Leave`/release
   notification back to the peer so macOS does not remain captured.

5. Gate cursor warp.
   `SetCursorPos` should be skipped while locked, and failure should be logged.
   Cursor warps on the secure desktop should not be treated as a valid handoff.

6. Keep macOS and Windows semantics symmetric.
   macOS capture suppresses crossings while the host screen is locked and now
   retries event-tap interruption. Windows should suppress local capture while
   locked and also report remote emulation unavailability while locked.

7. Normalize Korean IME toggle events at the Windows edge.
   Treat the Hangul/English key as one semantic toggle. Do not send both the
   translated Right Alt key and a separate Hangul toggle for the same physical
   action. Add a small diagnostic log that records the raw Windows virtual key,
   scan code, and emitted lan-mouse key code for Right Alt/Hangul transitions.

## Smoke tests for the Windows patch

- Windows unlocked, macOS unlocked: bidirectional handoff works.
- Windows locks while capturing macOS: Windows sends `AutoRelease`; macOS gets
  a `Leave` path and input returns locally.
- Windows locks while receiving macOS input: Windows flushes active emulation,
  refuses further `Input`, and macOS capture recovers without daemon restart.
- Both machines locked, then macOS unlocks first: macOS should not stay captured
  by a Windows peer that cannot receive input.
- Both machines locked, then Windows unlocks first: later handoff should work
  without restarting either daemon.
- Verify logs around `WTS_SESSION_LOCK`, `WTS_SESSION_UNLOCK`, `Enter`, `Ack`,
  `Leave`, `SendInput` failure, and recovery.
- On a Korean keyboard, press Hangul/English once while controlling macOS and
  verify exactly one macOS `Right Alt -> Korean IME toggle` log line and one
  resulting `TIS: switched to ...` line.

## Related local branches

The current deployed macOS branch is `kwen-mdns`. There is another local branch,
`kwen-mdns-hooks-mac-smoke`, with additional handoff hardening such as stale
capture-event filtering and modifier release fixes. Port those changes
selectively after the Windows lock-state work is scoped; avoid merging the whole
branch blindly because it contains multiple behavioral changes.
