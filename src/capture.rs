use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
#[cfg(target_os = "macos")]
use input_capture::error::MacosCaptureCreationError;
use input_capture::{
    CaptureCreationError, CaptureError, CaptureEvent, CaptureHandle, InputCapture,
    InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::{LanMouseConnection, LanMouseConnectionError};

const CAPTURE_RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const CAPTURE_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// The local cursor crossed a capture boundary.
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A remote client acknowledged capture and is ready for input.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered after the transport handoff completed.
    ClientEntered(u64),
    /// The local cursor reclaimed input from a remote client
    /// (peer sent a `Leave` — either they released their own
    /// outbound capture or are taking over). Mirror of
    /// [`ICaptureEvent::ClientEntered`] for the leave side.
    ClientLeft(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// release because the remote peer is taking over (they sent
    /// Enter+CursorPos). Skips the host-side warp so the peer's
    /// proportional CursorPos warp doesn't get clobbered by a
    /// racing local warp computed from stale virtual_cursor state.
    ReleaseForHandover,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// set the auto-release pixel threshold (macOS only). 0 disables.
    SetReleaseThreshold(u32),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
        release_threshold_px: u32,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            release_threshold_px: Rc::new(RefCell::new(release_threshold_px)),
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release_for_handover(&self) {
        self.request_tx
            .send(CaptureRequest::ReleaseForHandover)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    pub(crate) fn set_release_threshold(&mut self, threshold: u32) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetReleaseThreshold(threshold));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    release_threshold_px: Rc<RefCell<u32>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_capture(&self, handle: CaptureHandle) -> Option<(Position, CaptureType)> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, pos, capture_type)| (*pos, *capture_type))
    }

    fn has_capture(&self, handle: CaptureHandle) -> bool {
        self.captures.iter().any(|(h, ..)| *h == handle)
    }

    async fn ensure_ready_for_begin(&self, handle: CaptureHandle) -> bool {
        const TIMEOUT: Duration = Duration::from_millis(1200);
        const POLL: Duration = Duration::from_millis(25);

        if self.conn.is_ready(handle).await {
            return true;
        }
        self.conn.ensure_connected(handle).await;

        log::info!("client {handle} is not ready yet; waiting for initial connection");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                _ = self.cancellation_token.cancelled() => return false,
            }

            if self.conn.is_ready(handle).await {
                return true;
            }

            if Instant::now() >= deadline {
                return self.conn.is_ready(handle).await;
            }
        }
    }

    async fn run(mut self) {
        let mut retry_delay = CAPTURE_RETRY_INITIAL_DELAY;
        loop {
            match self.do_capture().await {
                Ok(()) => {
                    retry_delay = CAPTURE_RETRY_INITIAL_DELAY;
                }
                Err(e) => {
                    let should_auto_retry = should_auto_retry_capture(&e);
                    log::warn!("input capture exited: {e}");

                    if should_auto_retry {
                        let delay = retry_delay;
                        retry_delay = (retry_delay.saturating_mul(2)).min(CAPTURE_RETRY_MAX_DELAY);
                        log::info!(
                            "input capture will auto-restart in {}s after recoverable macOS event-tap interruption",
                            delay.as_secs()
                        );
                        if self.wait_for_reenable_or_retry(Some(delay)).await {
                            continue;
                        }
                        return;
                    }

                    retry_delay = CAPTURE_RETRY_INITIAL_DELAY;
                }
            }

            if !self.wait_for_reenable_or_retry(None).await {
                return;
            }
        }
    }

    async fn wait_for_reenable_or_retry(&mut self, retry_after: Option<Duration>) -> bool {
        let retry_sleep = retry_after.map(tokio::time::sleep);
        tokio::pin!(retry_sleep);

        loop {
            tokio::select! {
                _ = async {
                    match retry_sleep.as_mut().as_pin_mut() {
                        Some(sleep) => sleep.await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return true;
                }
                r = self.request_rx.recv() => match r.expect("channel closed") {
                    CaptureRequest::Reenable => return true,
                    CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                    CaptureRequest::Destroy(h) => self.remove_capture(h),
                    CaptureRequest::ReleaseForHandover => { /* nothing to do */ }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::SetReleaseThreshold(threshold) => {
                        *self.release_threshold_px.borrow_mut() = threshold;
                    }
                },
                _ = self.cancellation_token.cancelled() => return false,
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        // Push the configured auto-release threshold to the freshly
        // created InputCapture. The wall-press detection is
        // cross-platform — every backend benefits.
        capture.set_release_threshold(*self.release_threshold_px.borrow());

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(Ok(event)) => self.handle_capture_event(capture, event).await?,
                    Some(Err(e)) => {
                        // Backend died mid-session (e.g. macOS event
                        // tap disabled). Drain held keys and tell the
                        // peer we're gone before propagating, so the
                        // guest isn't left entered with stuck input
                        // while we tear down and auto-retry.
                        self.notify_peer_of_leave(capture).await;
                        return Err(e.into());
                    }
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    if !self.has_capture(handle) {
                        log::debug!("ignoring connection event for unknown capture {handle}");
                        continue;
                    }

                    match event {
                        // Connection acknowledged => input handoff completed.
                        // Only now notify the service layer so side effects such
                        // as monitor input switching do not run before the peer
                        // is actually ready to receive cursor/input events.
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            let was_waiting_for_this_client = self.state == State::WaitingForAck
                                && self.active_client == Some(handle);
                            self.state = State::Sending;
                            if was_waiting_for_this_client {
                                self.event_tx
                                    .send(ICaptureEvent::ClientEntered(handle))
                                    .expect("channel closed");
                            }
                        }
                        // Peer sent Leave — either they just released
                        // their own outbound capture, or they're
                        // taking over and want us to stop sending to
                        // them. The "taking over" case is the common
                        // one (CaptureBegin on the peer triggers a
                        // send_leave_event to every incoming address)
                        // and is followed by Enter+CursorPos from the
                        // peer. Use the handover release so the local
                        // host warp doesn't race against the peer's
                        // upcoming proportional CursorPos warp on our
                        // shared cursor.
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture_handover(capture).await?;
                            self.event_tx
                                .send(ICaptureEvent::ClientLeft(handle))
                                .expect("channel closed");
                        },
                        // Peer reported its display geometry — cache it
                        // so the wall-press model has a real upper
                        // clamp on virtual_pos for this position.
                        ProtoEvent::Bounds { width, height } => {
                            if let Some((pos, _)) = self.get_capture(handle) {
                                capture.set_peer_bounds(pos, width, height);
                            } else {
                                log::debug!("ignoring Bounds from removed capture {handle}");
                            }
                        }
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::ReleaseForHandover => self.release_capture_handover(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        if let Some((pos, _)) = self.get_capture(h) {
                            self.remove_capture(h);
                            capture.destroy(h).await?;
                            // Drop the cached geometry — the next client
                            // added at this position may report different
                            // bounds.
                            capture.clear_peer_bounds(pos);
                        } else {
                            log::debug!("ignoring destroy for removed capture {h}");
                        }
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::SetReleaseThreshold(threshold) => {
                        *self.release_threshold_px.borrow_mut() = threshold;
                        capture.set_release_threshold(threshold);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        let Some((capture_pos, capture_type)) = self.get_capture(handle) else {
            log::debug!("ignoring event for removed capture {handle}: {event:?}");
            return Ok(());
        };

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        // Backend self-released (currently only macOS, when sustained
        // back-toward-host motion crosses the configured threshold).
        // Drive the same teardown path as the release-bind chord so
        // the peer gets a Leave + key-up flush.
        if matches!(event, CaptureEvent::AutoRelease) {
            log::info!("releasing capture: backend auto-release");
            return self.release_capture(capture).await;
        }

        if matches!(event, CaptureEvent::Begin { .. }) {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if capture_type == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(capture_pos) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        if matches!(event, CaptureEvent::Begin { .. }) && !self.ensure_ready_for_begin(handle).await
        {
            log::info!("releasing capture: client {handle} is not ready yet");
            capture.release().await?;
            return Ok(());
        }

        // activated a new client
        if matches!(event, CaptureEvent::Begin { .. }) && Some(handle) != self.active_client {
            self.state = State::WaitingForAck;
            self.active_client.replace(handle);
        }

        let opposite_pos = to_proto_pos(capture_pos.opposite());

        // If we're starting a fresh capture and the backend reported
        // a cursor position at the moment of crossing, send a
        // `CursorPos` (host-normalized fraction + entry side from the
        // peer's frame) right after Enter. The peer scales against
        // its own live bounds and pins the on-axis dimension to the
        // matching edge — self-sufficient, no prior `Bounds`
        // round-trip needed, so the very first crossing also lands
        // the cursor at the visually-corresponding point.
        let cursor_pos = if let CaptureEvent::Begin {
            cursor: Some(cursor),
        } = event
        {
            capture.host_normalized_cursor(cursor).map(|(nx, ny)| {
                let proto_pos = to_proto_pos(capture_pos.opposite());
                (proto_pos, nx, ny)
            })
        } else {
            None
        };

        let proto_event = match event {
            CaptureEvent::Begin { .. } => ProtoEvent::Enter(opposite_pos),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos),
                State::Sending => ProtoEvent::Input(e),
            },
            CaptureEvent::AutoRelease => unreachable!("handled in early return above"),
        };

        if let Err(e) = self.conn.send(proto_event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            if matches!(e, LanMouseConnectionError::TargetEmulationDisabled)
                && can_drop_while_target_emulation_disabled(event)
            {
                debounce!(
                    PREV_LOG,
                    DUR,
                    log::debug!("dropping captured input for client {handle}: {e}")
                );
                return Ok(());
            }
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // Full teardown, not just a backend release: leaving
            // active_client/state set means the next crossing into the
            // same client skips the WaitingForAck transition and sends
            // Input without Enter retransmission or Ack gating.
            // notify_peer_of_leave only logs send failures, so this is
            // safe even though the transport just errored.
            self.release_capture(capture).await?;
            return Ok(());
        }

        // Send CursorPos right after Enter so the receiver can warp
        // its cursor to the visually-corresponding point on its own
        // screen — overrides the entry-edge-midpoint warp the
        // receiver otherwise applies on Enter.
        if let Some((pos, nx, ny)) = cursor_pos {
            if let Err(e) = self
                .conn
                .send(ProtoEvent::CursorPos { pos, nx, ny }, handle)
                .await
            {
                log::warn!("CursorPos send failed: {e}");
            }
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.notify_peer_of_leave(capture).await;
        capture.release().await
    }

    /// Release path used when the peer is taking over (they sent
    /// Enter+CursorPos). Same teardown — synthesize key-ups, reset
    /// mods, send Leave — but skip the host-side cursor warp so it
    /// doesn't race against the peer's authoritative CursorPos
    /// warp on our shared cursor.
    async fn release_capture_handover(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), CaptureError> {
        self.notify_peer_of_leave(capture).await;
        capture.release_no_host_warp().await
    }

    async fn notify_peer_of_leave(&mut self, capture: &mut InputCapture) {
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn should_auto_retry_capture(error: &InputCaptureError) -> bool {
    matches!(
        error,
        InputCaptureError::Capture(CaptureError::EventTapDisabled)
            | InputCaptureError::Create(CaptureCreationError::MacOS(
                MacosCaptureCreationError::EventTapCreation
            ))
    )
}

#[cfg(not(target_os = "macos"))]
fn should_auto_retry_capture(_error: &InputCaptureError) -> bool {
    false
}

fn can_drop_while_target_emulation_disabled(event: CaptureEvent) -> bool {
    matches!(
        event,
        CaptureEvent::Input(Event::Pointer(
            PointerEvent::Motion { .. }
                | PointerEvent::Axis { .. }
                | PointerEvent::AxisDiscrete120 { .. }
        ))
    )
}

#[cfg(test)]
mod target_disabled_tests {
    use super::*;
    use input_event::BTN_LEFT;

    #[test]
    fn only_stateless_pointer_events_are_dropped_when_target_emulation_is_disabled() {
        assert!(can_drop_while_target_emulation_disabled(
            CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 1.0,
            }))
        ));
        assert!(can_drop_while_target_emulation_disabled(
            CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: 1.0,
            }))
        ));
        assert!(can_drop_while_target_emulation_disabled(
            CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120,
            }))
        ));

        assert!(!can_drop_while_target_emulation_disabled(
            CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 0,
            }))
        ));
        assert!(!can_drop_while_target_emulation_disabled(
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 0,
            }))
        ));
        assert!(!can_drop_while_target_emulation_disabled(
            CaptureEvent::Begin { cursor: None }
        ));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn event_tap_disabled_is_auto_recoverable_on_macos() {
        assert!(should_auto_retry_capture(&InputCaptureError::Capture(
            CaptureError::EventTapDisabled,
        )));
    }

    #[test]
    fn event_tap_creation_is_auto_recoverable_on_macos() {
        assert!(should_auto_retry_capture(&InputCaptureError::Create(
            CaptureCreationError::MacOS(MacosCaptureCreationError::EventTapCreation),
        )));
    }

    #[test]
    fn missing_accessibility_permission_is_not_auto_recoverable_on_macos() {
        assert!(!should_auto_retry_capture(&InputCaptureError::Create(
            CaptureCreationError::MacOS(MacosCaptureCreationError::AccessibilityPermission),
        )));
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
