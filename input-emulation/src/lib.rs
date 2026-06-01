use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
mod x11;

#[cfg(all(unix, feature = "wlroots", not(target_os = "macos")))]
mod wlroots;

#[cfg(all(unix, feature = "remote_desktop_portal", not(target_os = "macos")))]
mod xdg_desktop_portal;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

/// fallback input emulation (logs events)
mod dummy;
mod error;

pub type EmulationHandle = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(all(unix, feature = "wlroots", not(target_os = "macos")))]
    Wlroots,
    #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
    Libei,
    #[cfg(all(unix, feature = "remote_desktop_portal", not(target_os = "macos")))]
    Xdp,
    #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(all(unix, feature = "wlroots", not(target_os = "macos")))]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
            Backend::Libei => write!(f, "libei"),
            #[cfg(all(unix, feature = "remote_desktop_portal", not(target_os = "macos")))]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
    pressed_buttons: HashMap<EmulationHandle, HashSet<u32>>,
}

impl InputEmulation {
    async fn with_backend(backend: Backend) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(all(unix, feature = "wlroots", not(target_os = "macos")))]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(all(unix, feature = "remote_desktop_portal", not(target_os = "macos")))]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(Self {
            emulation,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
        })
    }

    pub async fn new(backend: Option<Backend>) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(all(unix, feature = "wlroots", not(target_os = "macos")))]
            Backend::Wlroots,
            #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
            Backend::Libei,
            #[cfg(all(unix, feature = "remote_desktop_portal", not(target_os = "macos")))]
            Backend::Xdp,
            #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            match Self::with_backend(backend).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                // prevent double pressed / released keys
                if self.update_pressed_keys(handle, key, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                // prevent duplicate mouse button transitions and keep
                // enough state to release a stuck drag on session end
                if self.update_pressed_buttons(handle, button, state) {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.pressed_buttons.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        let _ = self.release_buttons(handle).await;
        let _ = self.release_keys(handle).await;
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.pressed_buttons.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    /// Display geometry of this device (union of all active
    /// displays), if the backend can report it. See
    /// `Emulation::display_bounds`.
    pub fn display_bounds(&self) -> Option<(u32, u32)> {
        self.emulation.display_bounds()
    }

    /// Warp the local cursor to the given absolute position. See
    /// `Emulation::warp_cursor`.
    pub async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        self.emulation.warp_cursor(x, y).await
    }

    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(keys) = self.pressed_keys.get_mut(&handle) {
            let keys = keys.drain().collect::<Vec<_>>();
            for key in keys {
                let event = Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                    log::warn!("releasing stuck key: {key:?}");
                }
            }
        }

        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        self.emulation.consume(event, handle).await?;
        Ok(())
    }

    pub async fn release_buttons(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(buttons) = self.pressed_buttons.get_mut(&handle) {
            let buttons = buttons.drain().collect::<Vec<_>>();
            for button in buttons {
                let event = Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button,
                    state: 0,
                });
                self.emulation.consume(event, handle).await?;
                log::warn!("releasing stuck mouse button: {button}");
            }
        }

        Ok(())
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// update the pressed_keys for the given handle
    /// returns whether the event should be processed
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> bool {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return false;
        };

        if state == 0 {
            // currently pressed => can release
            pressed_keys.remove(&key)
        } else {
            // currently not pressed => can press
            pressed_keys.insert(key)
        }
    }

    /// update the pressed mouse buttons for the given handle
    /// returns whether the event should be processed
    fn update_pressed_buttons(&mut self, handle: EmulationHandle, button: u32, state: u32) -> bool {
        let Some(pressed_buttons) = self.pressed_buttons.get_mut(&handle) else {
            return false;
        };

        match state {
            // currently pressed => can release
            0 => pressed_buttons.remove(&button),
            // currently not pressed => can press
            1 => pressed_buttons.insert(button),
            // unknown button state: forward it unchanged
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::BTN_LEFT;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingEmulation {
        events: Arc<Mutex<Vec<(Event, EmulationHandle)>>>,
    }

    #[async_trait::async_trait]
    impl Emulation for RecordingEmulation {
        async fn consume(
            &mut self,
            event: Event,
            handle: EmulationHandle,
        ) -> Result<(), EmulationError> {
            self.events.lock().unwrap().push((event, handle));
            Ok(())
        }

        async fn create(&mut self, _handle: EmulationHandle) {}
        async fn destroy(&mut self, _handle: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }

    fn recording_input_emulation(
        events: Arc<Mutex<Vec<(Event, EmulationHandle)>>>,
    ) -> InputEmulation {
        InputEmulation {
            emulation: Box::new(RecordingEmulation { events }),
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
        }
    }

    fn left_button(state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state,
        })
    }

    #[tokio::test]
    async fn destroy_releases_pressed_pointer_buttons() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut emulation = recording_input_emulation(events.clone());

        emulation.create(7).await;
        emulation.consume(left_button(1), 7).await.unwrap();
        emulation.destroy(7).await;

        let events = events.lock().unwrap().clone();
        assert_eq!(events[0], (left_button(1), 7));
        assert!(events.contains(&(left_button(0), 7)));
    }

    #[tokio::test]
    async fn duplicate_pointer_button_transitions_are_suppressed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut emulation = recording_input_emulation(events.clone());

        emulation.create(7).await;
        emulation.consume(left_button(1), 7).await.unwrap();
        emulation.consume(left_button(1), 7).await.unwrap();
        emulation.consume(left_button(0), 7).await.unwrap();
        emulation.consume(left_button(0), 7).await.unwrap();

        let events = events.lock().unwrap().clone();
        assert_eq!(events, vec![(left_button(1), 7), (left_button(0), 7)]);
    }
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);

    /// Geometry (width, height) of the union of this device's
    /// active displays in pixels. Used by the protocol-level
    /// `Bounds` event so a capturing peer can model the guest
    /// cursor's position. Backends that can't report geometry
    /// should leave the default `None` and the wall-press
    /// auto-release fallback will degrade to "no upper clamp"
    /// behavior on the host.
    fn display_bounds(&self) -> Option<(u32, u32)> {
        None
    }

    /// Warp the cursor to an absolute position on the receiving
    /// device's primary display, if the backend supports absolute
    /// positioning. Called when an `Enter` event arrives so the
    /// guest cursor lands at the entry edge instead of staying
    /// wherever the previous capture session left it. Backends
    /// without absolute positioning can leave the default no-op
    /// — the wall-press auto-release will be inaccurate but the
    /// connection still works.
    async fn warp_cursor(&mut self, _x: i32, _y: i32) -> Result<(), EmulationError> {
        Ok(())
    }
}
