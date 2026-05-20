use super::{Emulation, EmulationHandle, error::EmulationError};
use async_trait::async_trait;
use bitflags::bitflags;
use core_graphics::base::CGFloat;
use core_graphics::display::{
    CGDirectDisplayID, CGDisplay, CGDisplayBounds, CGGetDisplaysWithRect, CGPoint, CGRect, CGSize,
};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};
use keycode::{KeyMap, KeyMapping};
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{sync::Notify, task::JoinHandle};

use super::error::MacOSEmulationCreationError;

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const IME_TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Per-axis scale applied to incoming pointer motion deltas before they
/// are turned into mouse-move events. macOS's `mouse.scaling` preference
/// only affects HID devices, not synthetic CGEvents posted by lan-mouse,
/// so the only reliable way to slow down the remote cursor is to scale
/// the deltas here. 1.0 = original speed.
const MOUSE_SPEED_MULTIPLIER: f64 = 0.7;

pub(crate) struct MacOSEmulation {
    /// global event source for all events
    event_source: CGEventSource,
    /// task handle for key repeats
    repeat_task: Option<JoinHandle<()>>,
    /// current state of the mouse buttons (tracked by evdev button code)
    pressed_buttons: HashSet<u32>,
    /// extra buttons whose press was routed to a synthetic key (F9/F11).
    /// The matching release must be swallowed so the host app never sees
    /// a phantom mouseUp.
    synth_keyed_buttons: HashSet<u32>,
    /// button previously pressed (evdev button code)
    previous_button: Option<u32>,
    /// timestamp of previous click (button down)
    previous_button_click: Option<Instant>,
    /// click state, i.e. number of clicks in quick succession
    button_click_state: i64,
    /// current modifier state
    modifier_state: Rc<Cell<XMods>>,
    /// last accepted remote IME toggle time
    last_ime_toggle: Option<Instant>,
    /// notify to cancel key repeats
    notify_repeat_task: Arc<Notify>,
}

/// Maps an evdev button code to the CGEventType used for drag events.
fn drag_event_type(button: u32) -> CGEventType {
    match button {
        BTN_LEFT => CGEventType::LeftMouseDragged,
        BTN_RIGHT => CGEventType::RightMouseDragged,
        // middle, back, forward, and any other button all use OtherMouseDragged
        _ => CGEventType::OtherMouseDragged,
    }
}

unsafe impl Send for MacOSEmulation {}

impl MacOSEmulation {
    pub(crate) fn new() -> Result<Self, MacOSEmulationCreationError> {
        request_macos_emulation_permissions()?;

        let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| MacOSEmulationCreationError::EventSourceCreation)?;
        Ok(Self {
            event_source,
            pressed_buttons: HashSet::new(),
            synth_keyed_buttons: HashSet::new(),
            previous_button: None,
            previous_button_click: None,
            button_click_state: 0,
            repeat_task: None,
            notify_repeat_task: Arc::new(Notify::new()),
            modifier_state: Rc::new(Cell::new(XMods::empty())),
            last_ime_toggle: None,
        })
    }

    fn get_mouse_location(&self) -> Option<CGPoint> {
        let event: CGEvent = CGEvent::new(self.event_source.clone()).ok()?;
        Some(event.location())
    }

    async fn spawn_repeat_task(&mut self, key: u16) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.cancel_repeat_task().await;
        // initial key event
        key_event(self.event_source.clone(), key, 1, self.modifier_state.get());
        // repeat task
        let event_source = self.event_source.clone();
        let notify = self.notify_repeat_task.clone();
        let modifiers = self.modifier_state.clone();
        let repeat_task = tokio::task::spawn_local(async move {
            let stop = tokio::select! {
                _ = tokio::time::sleep(DEFAULT_REPEAT_DELAY) => false,
                _ = notify.notified() => true,
            };
            if !stop {
                loop {
                    key_event(event_source.clone(), key, 1, modifiers.get());
                    tokio::select! {
                        _ = tokio::time::sleep(DEFAULT_REPEAT_INTERVAL) => {},
                        _ = notify.notified() => break,
                    }
                }
            }
            // release key when cancelled
            update_modifiers(&modifiers, key as u32, 0);
            key_event(event_source.clone(), key, 0, modifiers.get());
        });
        self.repeat_task = Some(repeat_task);
    }

    fn accept_ime_toggle(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_ime_toggle
            .is_some_and(|last| now.duration_since(last) < IME_TOGGLE_DEBOUNCE)
        {
            log::debug!("Right Alt -> Korean IME toggle ignored by debounce");
            return false;
        }
        self.last_ime_toggle = Some(now);
        true
    }

    async fn cancel_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            self.notify_repeat_task.notify_waiters();
            let _ = task.await;
        }
    }
}

fn request_macos_emulation_permissions() -> Result<(), MacOSEmulationCreationError> {
    // Request both permissions up front so the user sees both TCC prompts
    // on the first launch. See the matching comment in input-capture/src/
    // macos.rs::request_macos_capture_permissions for the rationale.
    let accessibility = request_accessibility_permission();
    let input_control = request_input_control_permission();

    if !accessibility {
        return Err(MacOSEmulationCreationError::AccessibilityPermission);
    }
    if !input_control {
        return Err(MacOSEmulationCreationError::InputControlPermission);
    }
    Ok(())
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see lan_mouse_gtk::macos_privacy).
    unsafe { AXIsProcessTrusted() }
}

fn request_input_control_permission() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// Text Input Source (TIS) bindings for direct IME switching. macOS rejects
// synthetic input-source shortcuts (e.g. Ctrl+Space, F18) coming from
// CGEventPost, so we toggle the input source programmatically instead.
type TISInputSourceRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFIndex = isize;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

// TIS symbols live in Carbon.framework/Frameworks/HIToolbox.framework.
// The SDK forbids linking HIToolbox directly, and the C-side static
// `kTISPropertyInputSourceID` is bound eagerly by dyld at process start,
// before any user code can dlopen the subframework. So everything—both
// the function symbols and the property-key constant—is resolved at
// runtime via dlsym after the first dlopen.
unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, sym: *const std::ffi::c_char) -> *mut c_void;
    fn dlerror() -> *const std::ffi::c_char;
}
const RTLD_LAZY: i32 = 1;
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

type FnTisCopyCurrent = unsafe extern "C" fn() -> TISInputSourceRef;
type FnTisCopyForLanguage = unsafe extern "C" fn(CFStringRef) -> TISInputSourceRef;
type FnTisCreateList = unsafe extern "C" fn(*const c_void, bool) -> CFArrayRef;
type FnTisGetProperty = unsafe extern "C" fn(TISInputSourceRef, CFStringRef) -> *const c_void;
type FnTisSelect = unsafe extern "C" fn(TISInputSourceRef) -> i32;

struct TisApi {
    copy_current: FnTisCopyCurrent,
    copy_for_language: FnTisCopyForLanguage,
    create_list: Option<FnTisCreateList>,
    get_property: FnTisGetProperty,
    select: FnTisSelect,
    property_input_source_id: CFStringRef,
    property_input_source_languages: CFStringRef,
    property_input_source_is_select_capable: CFStringRef,
}

unsafe impl Send for TisApi {}
unsafe impl Sync for TisApi {}

static TIS: std::sync::OnceLock<Option<TisApi>> = std::sync::OnceLock::new();

fn tis_api() -> Option<&'static TisApi> {
    TIS.get_or_init(|| unsafe {
        let path = c"/System/Library/Frameworks/Carbon.framework/Versions/Current/Frameworks/HIToolbox.framework/HIToolbox";
        let handle = dlopen(path.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            let err = dlerror();
            let msg = if err.is_null() {
                "<no dlerror>".to_string()
            } else {
                let mut len = 0;
                while *err.add(len) != 0 {
                    len += 1;
                }
                std::str::from_utf8(std::slice::from_raw_parts(err as *const u8, len))
                    .unwrap_or("<invalid utf8>")
                    .to_string()
            };
            log::warn!("TIS: dlopen HIToolbox failed: {msg}; falling back to RTLD_DEFAULT");
        }
        let lookup_handle = if handle.is_null() { RTLD_DEFAULT } else { handle };

        let copy_current = dlsym(lookup_handle, c"TISCopyCurrentKeyboardInputSource".as_ptr());
        let copy_for_lang = dlsym(lookup_handle, c"TISCopyInputSourceForLanguage".as_ptr());
        // Current SDKs no longer export TISCopyInputSourceList, but the
        // older TISCreateInputSourceList alias is still available.
        let mut create_list = dlsym(lookup_handle, c"TISCreateInputSourceList".as_ptr());
        if create_list.is_null() {
            create_list = dlsym(lookup_handle, c"TISCopyInputSourceList".as_ptr());
        }
        let get_property = dlsym(lookup_handle, c"TISGetInputSourceProperty".as_ptr());
        let select = dlsym(lookup_handle, c"TISSelectInputSource".as_ptr());
        let id_var = dlsym(lookup_handle, c"kTISPropertyInputSourceID".as_ptr());
        let languages_var = dlsym(lookup_handle, c"kTISPropertyInputSourceLanguages".as_ptr());
        let selectable_var = dlsym(
            lookup_handle,
            c"kTISPropertyInputSourceIsSelectCapable".as_ptr(),
        );

        if copy_current.is_null()
            || copy_for_lang.is_null()
            || get_property.is_null()
            || select.is_null()
            || id_var.is_null()
            || languages_var.is_null()
            || selectable_var.is_null()
        {
            log::warn!("TIS: required symbols missing; IME toggle disabled");
            return None;
        }

        // These vars are pointers to static CFStringRef variables.
        let property_input_source_id = *(id_var as *const CFStringRef);
        let property_input_source_languages = *(languages_var as *const CFStringRef);
        let property_input_source_is_select_capable = *(selectable_var as *const CFStringRef);

        Some(TisApi {
            copy_current: std::mem::transmute::<*mut c_void, FnTisCopyCurrent>(copy_current),
            copy_for_language: std::mem::transmute::<*mut c_void, FnTisCopyForLanguage>(
                copy_for_lang,
            ),
            create_list: if create_list.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut c_void, FnTisCreateList>(
                    create_list,
                ))
            },
            get_property: std::mem::transmute::<*mut c_void, FnTisGetProperty>(get_property),
            select: std::mem::transmute::<*mut c_void, FnTisSelect>(select),
            property_input_source_id,
            property_input_source_languages,
            property_input_source_is_select_capable,
        })
    })
    .as_ref()
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFBooleanGetValue(boolean: CFBooleanRef) -> bool;
    fn CFStringGetCString(s: CFStringRef, buf: *mut u8, len: CFIndex, encoding: u32) -> bool;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFArrayGetCount(arr: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: CFIndex) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(num: *const c_void, the_type: i64, value_ptr: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    static kCGWindowOwnerName: CFStringRef;
    static kCGWindowLayer: CFStringRef;
}

const K_CF_NUMBER_SINT32_TYPE: i64 = 3;
const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

/// Returns the owner-name of the topmost on-screen regular-app window
/// (layer == 0). Used to route mouse4/mouse5 differently depending on
/// the frontmost app (browsers/Finder keep back-forward semantics).
fn frontmost_window_owner_name() -> Option<String> {
    unsafe {
        let arr = CGWindowListCopyWindowInfo(
            K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
            0,
        );
        if arr.is_null() {
            return None;
        }
        let mut result: Option<String> = None;
        let count = CFArrayGetCount(arr);
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(arr, i);
            if dict.is_null() {
                continue;
            }
            let layer_val = CFDictionaryGetValue(dict, kCGWindowLayer as *const c_void);
            if layer_val.is_null() {
                continue;
            }
            let mut layer: i32 = 0;
            if !CFNumberGetValue(
                layer_val,
                K_CF_NUMBER_SINT32_TYPE,
                &mut layer as *mut i32 as *mut c_void,
            ) {
                continue;
            }
            if layer != 0 {
                continue;
            }
            let name_val = CFDictionaryGetValue(dict, kCGWindowOwnerName as *const c_void);
            if name_val.is_null() {
                continue;
            }
            let mut buf = [0u8; 256];
            if !CFStringGetCString(
                name_val as CFStringRef,
                buf.as_mut_ptr(),
                buf.len() as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
            ) {
                continue;
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            result = std::str::from_utf8(&buf[..end]).ok().map(|s| s.to_string());
            break;
        }
        CFRelease(arr);
        result
    }
}

/// How a mouse4/mouse5 press should be routed based on the frontmost app.
enum BackForwardRoute {
    /// Forward the press as a standard OtherMouse button 3/4 event — the app
    /// handles it natively (browsers).
    Passthrough,
    /// Synthesize ⌘+[ / ⌘+] — the app exposes navigation only via that menu
    /// shortcut, not via raw side-buttons (Finder).
    CmdBracket,
    /// Trigger Mission Control / Show Desktop via a trusted path
    /// (`open -a` / AppleScript System Events).
    SystemShortcut,
}

fn back_forward_route(name: Option<&str>) -> BackForwardRoute {
    match name {
        Some("Google Chrome" | "Safari") => BackForwardRoute::Passthrough,
        Some("Finder") => BackForwardRoute::CmdBracket,
        _ => BackForwardRoute::SystemShortcut,
    }
}

/// Synthesizes a ⌘+<key> chord. Used for Finder back/forward (⌘+[ and ⌘+]).
/// App menu shortcuts are accepted from synthetic CGEvents (only *system*
/// shortcut triggers like F9/F11 get rejected).
///
/// Wraps the chord with explicit FlagsChanged events so the system sees a
/// clean ⌘-press / ⌘-release boundary — without the final release event,
/// macOS leaves the modifier stuck, and the next left click is interpreted
/// as ⌘+Click (no window focus, action happens in place, looks like a
/// drag-without-focus bug).
fn send_cmd_key(event_source: CGEventSource, mac_keycode: u16, current_mods: XMods) {
    let mods_with_cmd = to_cgevent_flags(current_mods) | CGEventFlags::CGEventFlagCommand;
    let mods_restored = to_cgevent_flags(current_mods);

    // 1. Tell the system ⌘ is now down.
    if let Ok(e) = CGEvent::new(event_source.clone()) {
        e.set_type(CGEventType::FlagsChanged);
        e.set_flags(mods_with_cmd);
        e.post(CGEventTapLocation::HID);
    }
    // 2. Key down.
    if let Ok(e) = CGEvent::new_keyboard_event(event_source.clone(), mac_keycode, true) {
        e.set_flags(mods_with_cmd);
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("send_cmd_key: keydown creation failed");
    }
    // 3. Key up.
    if let Ok(e) = CGEvent::new_keyboard_event(event_source.clone(), mac_keycode, false) {
        e.set_flags(mods_with_cmd);
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("send_cmd_key: keyup creation failed");
    }
    // 4. Release ⌘ — restore the modifier state lan-mouse was tracking.
    if let Ok(e) = CGEvent::new(event_source) {
        e.set_type(CGEventType::FlagsChanged);
        e.set_flags(mods_restored);
        e.post(CGEventTapLocation::HID);
    }
}

/// Triggers Mission Control by posting F9 from a `HIDSystemState` event
/// source — the same low-level path Show Desktop uses (see
/// `trigger_show_desktop`). HID-state events participate in WindowServer's
/// mid-transition reverse handler, so a second click cancels an in-flight
/// transition instead of waiting for the animation to finish.
fn trigger_mission_control() {
    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("trigger_mission_control: HID source creation failed");
            return;
        }
    };
    const KEY_F9: u16 = 0x65;
    if let Ok(e) = CGEvent::new_keyboard_event(source.clone(), KEY_F9, true) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("trigger_mission_control: keydown creation failed");
        return;
    }
    if let Ok(e) = CGEvent::new_keyboard_event(source, KEY_F9, false) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("trigger_mission_control: keyup creation failed");
    }
}

/// Toggles the Korean IME by synthesizing a CapsLock press from a
/// `HIDSystemState` source. Currently inactive — see analysis below: macOS
/// treats CapsLock as a modifier *flag*, not a key, so a plain keyDown/keyUp
/// CGEvent doesn't change the lock state even at HID-state priority. Kept
/// for future experimentation (e.g. paired with a `FlagsChanged`
/// `CGEventFlagAlphaShift` event).
#[allow(dead_code)]
fn toggle_korean_via_capslock() {
    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("toggle_korean_via_capslock: HID source creation failed");
            return;
        }
    };
    const KEY_CAPSLOCK: u16 = 0x39;
    if let Ok(e) = CGEvent::new_keyboard_event(source.clone(), KEY_CAPSLOCK, true) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("toggle_korean_via_capslock: keydown creation failed");
        return;
    }
    if let Ok(e) = CGEvent::new_keyboard_event(source, KEY_CAPSLOCK, false) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("toggle_korean_via_capslock: keyup creation failed");
    }
}

/// Triggers Show Desktop by posting F11 from a `HIDSystemState` CGEventSource,
/// the event-source state that sits closest to a real HID interrupt. The hope
/// is that WindowServer's mid-transition reverse handler (which lets native
/// F11 cancel an in-flight Show Desktop transition) accepts this source. The
/// AppleScript path we used before is trusted, but its events arrive too high
/// in the stack to participate in transition-reverse, so the user only gets
/// the second toggle *after* the first transition finishes.
///
/// If the HID-state path turns out to be rejected by the shortcut handler,
/// swap back to `trigger_show_desktop_via_applescript()` below.
fn trigger_show_desktop() {
    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("trigger_show_desktop: HID source creation failed");
            return;
        }
    };
    const KEY_F11: u16 = 0x67;
    if let Ok(e) = CGEvent::new_keyboard_event(source.clone(), KEY_F11, true) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("trigger_show_desktop: keydown creation failed");
        return;
    }
    if let Ok(e) = CGEvent::new_keyboard_event(source, KEY_F11, false) {
        e.post(CGEventTapLocation::HID);
    } else {
        log::warn!("trigger_show_desktop: keyup creation failed");
    }
}

/// Legacy trigger kept around in case the HID-state path turns out to be
/// rejected by the system shortcut handler. Routes through NSAppleScript /
/// osascript (trusted, but high in the stack — no transition-reverse).
#[allow(dead_code)]
fn trigger_show_desktop_via_applescript() {
    std::thread::spawn(|| {
        const SCRIPT: &str = "tell application \"System Events\" to key code 103";
        if run_apple_script_in_process(SCRIPT) {
            return;
        }
        log::debug!("trigger_show_desktop: falling back to osascript subprocess");
        let result = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", SCRIPT])
            .status();
        if let Err(e) = result {
            log::warn!("trigger_show_desktop: subprocess spawn failed: {e}");
        }
    });
}

// ---- NSAppleScript in-process via Objective-C runtime + dlsym -------------
//
// Spawning `osascript` costs 200-300ms (process fork + Apple Event runtime
// initialization). We can drop that to <10ms by going through Foundation's
// NSAppleScript class directly. Symbols are resolved at runtime (matching
// the same dlsym pattern used for the TIS Korean-IME toggle) so we add no
// new build dependencies. The first call still pays the AppleScript runtime
// warmup, but subsequent calls are fast.

type ObjcId = *const c_void;
type ObjcSel = *const c_void;
type ObjcClass = *const c_void;

type FnObjcGetClass = unsafe extern "C" fn(name: *const std::ffi::c_char) -> ObjcClass;
type FnSelRegisterName = unsafe extern "C" fn(name: *const std::ffi::c_char) -> ObjcSel;
type FnMsgSend0 = unsafe extern "C" fn(ObjcId, ObjcSel) -> ObjcId;
type FnMsgSend1 = unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId) -> ObjcId;
type FnMsgSendErr = unsafe extern "C" fn(ObjcId, ObjcSel, *mut ObjcId) -> ObjcId;

struct AppleScriptApi {
    nsapplescript_class: ObjcClass,
    nsautoreleasepool_class: ObjcClass,
    alloc_sel: ObjcSel,
    init_sel: ObjcSel,
    init_with_source_sel: ObjcSel,
    execute_sel: ObjcSel,
    release_sel: ObjcSel,
    drain_sel: ObjcSel,
    msg_send_0: FnMsgSend0,
    msg_send_1: FnMsgSend1,
    msg_send_err: FnMsgSendErr,
}

unsafe impl Send for AppleScriptApi {}
unsafe impl Sync for AppleScriptApi {}

static APPLESCRIPT_API: std::sync::OnceLock<Option<AppleScriptApi>> = std::sync::OnceLock::new();

fn applescript_api() -> Option<&'static AppleScriptApi> {
    APPLESCRIPT_API
        .get_or_init(|| unsafe {
            // Pull in Foundation so NSAppleScript is in dyld. Failure is
            // tolerated — Foundation is usually loaded transitively.
            let foundation_path = c"/System/Library/Frameworks/Foundation.framework/Foundation";
            let _foundation = dlopen(foundation_path.as_ptr(), RTLD_LAZY);

            let get_class = dlsym(RTLD_DEFAULT, c"objc_getClass".as_ptr());
            let sel_register = dlsym(RTLD_DEFAULT, c"sel_registerName".as_ptr());
            let msg_send = dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr());
            if get_class.is_null() || sel_register.is_null() || msg_send.is_null() {
                log::warn!("NSAppleScript: objc runtime missing; subprocess fallback only");
                return None;
            }
            let get_class: FnObjcGetClass = std::mem::transmute(get_class);
            let sel_register: FnSelRegisterName = std::mem::transmute(sel_register);
            let msg_send_0: FnMsgSend0 = std::mem::transmute(msg_send);
            let msg_send_1: FnMsgSend1 = std::mem::transmute(msg_send);
            let msg_send_err: FnMsgSendErr = std::mem::transmute(msg_send);

            let nsapplescript_class = get_class(c"NSAppleScript".as_ptr());
            let nsautoreleasepool_class = get_class(c"NSAutoreleasePool".as_ptr());
            if nsapplescript_class.is_null() || nsautoreleasepool_class.is_null() {
                log::warn!("NSAppleScript: required classes not found; subprocess fallback only");
                return None;
            }
            Some(AppleScriptApi {
                nsapplescript_class,
                nsautoreleasepool_class,
                alloc_sel: sel_register(c"alloc".as_ptr()),
                init_sel: sel_register(c"init".as_ptr()),
                init_with_source_sel: sel_register(c"initWithSource:".as_ptr()),
                execute_sel: sel_register(c"executeAndReturnError:".as_ptr()),
                release_sel: sel_register(c"release".as_ptr()),
                drain_sel: sel_register(c"drain".as_ptr()),
                msg_send_0,
                msg_send_1,
                msg_send_err,
            })
        })
        .as_ref()
}

// ---- NSEvent media-key synthesis via Objective-C runtime + dlsym ----------
//
// macOS routes hardware volume / mute / brightness / play-pause keys as
// `NSSystemDefined` events (type 14, subtype 8 — "auxiliary control
// buttons"), NOT as regular keyboard events. Posting CGKeyDown/KeyUp at the
// matching keycode (kVK_VolumeUp etc.) does NOT change volume or pop the
// OSD — the system shortcut handler ignores it.
//
// The well-known synthesis path is `+[NSEvent
// otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:
// subtype:data1:data2:]` followed by `-CGEvent` and CGEventPost. We reach
// that via objc_msgSend resolved at runtime (same pattern as NSAppleScript
// above), avoiding a new build dependency.

const NSEVENT_TYPE_SYSTEM_DEFINED: u64 = 14;
const NSEVENT_SUBTYPE_AUX_CONTROL: i64 = 8;
const NX_KEYTYPE_SOUND_UP: u32 = 0;
const NX_KEYTYPE_SOUND_DOWN: u32 = 1;
const NX_KEYTYPE_MUTE: u32 = 7;
const K_CG_HID_EVENT_TAP: u32 = 0;

#[repr(C)]
#[derive(Copy, Clone)]
struct NSPoint {
    x: f64,
    y: f64,
}

type FnNSEventOther = unsafe extern "C" fn(
    cls: ObjcClass,
    sel: ObjcSel,
    event_type: u64,
    location: NSPoint,
    modifier_flags: u64,
    timestamp: f64,
    window_number: i64,
    context: ObjcId,
    subtype: i16,
    data1: i64,
    data2: i64,
) -> ObjcId;

type FnNSEventCGEvent = unsafe extern "C" fn(self_: ObjcId, sel: ObjcSel) -> *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventPost(tap: u32, event: *const c_void);
}

struct MediaKeyApi {
    nsevent_class: ObjcClass,
    other_event_sel: ObjcSel,
    cg_event_sel: ObjcSel,
    other_event: FnNSEventOther,
    cg_event_of: FnNSEventCGEvent,
}

unsafe impl Send for MediaKeyApi {}
unsafe impl Sync for MediaKeyApi {}

static MEDIA_KEY_API: std::sync::OnceLock<Option<MediaKeyApi>> = std::sync::OnceLock::new();

fn media_key_api() -> Option<&'static MediaKeyApi> {
    MEDIA_KEY_API
        .get_or_init(|| unsafe {
            // Need AppKit loaded for NSEvent.
            let _appkit =
                dlopen(c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(), RTLD_LAZY);
            let get_class = dlsym(RTLD_DEFAULT, c"objc_getClass".as_ptr());
            let sel_register = dlsym(RTLD_DEFAULT, c"sel_registerName".as_ptr());
            let msg_send = dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr());
            if get_class.is_null() || sel_register.is_null() || msg_send.is_null() {
                log::warn!("media-key: objc runtime missing");
                return None;
            }
            let get_class: FnObjcGetClass = std::mem::transmute(get_class);
            let sel_register: FnSelRegisterName = std::mem::transmute(sel_register);
            let other_event: FnNSEventOther = std::mem::transmute(msg_send);
            let cg_event_of: FnNSEventCGEvent = std::mem::transmute(msg_send);

            let nsevent_class = get_class(c"NSEvent".as_ptr());
            if nsevent_class.is_null() {
                log::warn!("media-key: NSEvent class not found");
                return None;
            }
            let other_event_sel = sel_register(
                c"otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:".as_ptr(),
            );
            let cg_event_sel = sel_register(c"CGEvent".as_ptr());
            Some(MediaKeyApi {
                nsevent_class,
                other_event_sel,
                cg_event_sel,
                other_event,
                cg_event_of,
            })
        })
        .as_ref()
}

/// Synthesizes a media key press (keydown + keyup) via NSSystemDefined event.
/// `key_type` is one of NX_KEYTYPE_SOUND_UP / SOUND_DOWN / MUTE / PLAY etc.
/// This produces the system OSD that real hardware volume keys produce.
///
/// `fine_step` adds Shift+Option NSEvent modifier bits — equivalent to a
/// physical Shift+Option+VolumeKey on a Mac keyboard, which macOS interprets
/// as a 1/4-step (so the volume changes in 1/64 increments instead of 1/16).
/// `false` matches a plain hardware volume-key press.
fn post_media_key(key_type: u32, fine_step: bool) {
    // NSEventModifierFlag bits — only the higher-order ones are NSEvent
    // modifier flags; the lower 0xa00/0xb00 bits are the media-key magic.
    const NSEVENT_MOD_SHIFT: u64 = 1 << 17;
    const NSEVENT_MOD_OPTION: u64 = 1 << 19;
    let fine_mods: u64 = if fine_step {
        NSEVENT_MOD_SHIFT | NSEVENT_MOD_OPTION
    } else {
        0
    };

    let Some(media) = media_key_api() else {
        log::warn!("post_media_key: API unavailable");
        return;
    };
    // Borrow the NSAppleScript-side autorelease pool helpers so the events
    // we synthesize get released instead of accumulating per call.
    let pool_api = applescript_api();
    unsafe {
        let pool = pool_api.map(|p| {
            let alloc = (p.msg_send_0)(p.nsautoreleasepool_class, p.alloc_sel);
            if alloc.is_null() {
                std::ptr::null()
            } else {
                (p.msg_send_0)(alloc, p.init_sel)
            }
        });

        for &down in &[true, false] {
            let flags: u64 = (if down { 0xa00 } else { 0xb00 }) | fine_mods;
            // data1 encodes (keyType in upper 16 bits) | (down/up flag in lower 16).
            let state_nibble: i64 = if down { 0xa } else { 0xb };
            let data1: i64 = ((key_type as i64) << 16) | (state_nibble << 8);

            let nsevent = (media.other_event)(
                media.nsevent_class,
                media.other_event_sel,
                NSEVENT_TYPE_SYSTEM_DEFINED,
                NSPoint { x: 0.0, y: 0.0 },
                flags,
                0.0,
                0,
                std::ptr::null(),
                NSEVENT_SUBTYPE_AUX_CONTROL as i16,
                data1,
                -1,
            );
            if nsevent.is_null() {
                log::warn!("post_media_key: NSEvent creation returned nil");
                continue;
            }
            let cg_event = (media.cg_event_of)(nsevent, media.cg_event_sel);
            if cg_event.is_null() {
                log::warn!("post_media_key: -[NSEvent CGEvent] returned NULL");
                continue;
            }
            CGEventPost(K_CG_HID_EVENT_TAP, cg_event);
        }

        if let (Some(p), Some(pool_obj)) = (pool_api, pool) {
            if !pool_obj.is_null() {
                let _ = (p.msg_send_0)(pool_obj, p.drain_sel);
            }
        }
    }
}

/// Compiles and executes a one-shot AppleScript using NSAppleScript in this
/// process. Returns `true` on success; `false` if the runtime was unavailable
/// or the source couldn't be turned into an NSString. AppleScript runtime
/// errors are logged but still return `true` (we did execute — the script
/// itself failed).
fn run_apple_script_in_process(source: &str) -> bool {
    let Some(api) = applescript_api() else {
        return false;
    };
    let ns_string = cfstring_from(source);
    if ns_string.is_null() {
        return false;
    }
    unsafe {
        let pool_alloc = (api.msg_send_0)(api.nsautoreleasepool_class, api.alloc_sel);
        let pool = if pool_alloc.is_null() {
            std::ptr::null()
        } else {
            (api.msg_send_0)(pool_alloc, api.init_sel)
        };

        let script_alloc = (api.msg_send_0)(api.nsapplescript_class, api.alloc_sel);
        if script_alloc.is_null() {
            if !pool.is_null() {
                let _ = (api.msg_send_0)(pool, api.drain_sel);
            }
            CFRelease(ns_string);
            return false;
        }
        let script = (api.msg_send_1)(script_alloc, api.init_with_source_sel, ns_string as ObjcId);
        if script.is_null() {
            if !pool.is_null() {
                let _ = (api.msg_send_0)(pool, api.drain_sel);
            }
            CFRelease(ns_string);
            log::warn!("NSAppleScript: initWithSource: returned nil");
            return false;
        }
        let mut err: ObjcId = std::ptr::null();
        let _result = (api.msg_send_err)(script, api.execute_sel, &mut err);
        let _ = (api.msg_send_0)(script, api.release_sel);

        if !err.is_null() {
            log::warn!("NSAppleScript: execution returned an error dictionary");
        }

        if !pool.is_null() {
            let _ = (api.msg_send_0)(pool, api.drain_sel);
        }
        CFRelease(ns_string);
    }
    true
}

fn cfstring_from(s: &str) -> CFStringRef {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    unsafe {
        CFStringCreateWithCString(std::ptr::null(), bytes.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    }
}

fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    unsafe {
        let mut buf = [0u8; 256];
        if !CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
        ) {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..end]).ok().map(|s| s.to_string())
    }
}

fn input_source_id(api: &TisApi, source: TISInputSourceRef) -> Option<String> {
    unsafe {
        let id_ref = (api.get_property)(source, api.property_input_source_id);
        if id_ref.is_null() {
            return None;
        }
        cfstring_to_string(id_ref as CFStringRef)
    }
}

fn input_source_has_language(api: &TisApi, source: TISInputSourceRef, language: &str) -> bool {
    unsafe {
        let languages_ref = (api.get_property)(source, api.property_input_source_languages);
        if languages_ref.is_null() {
            return false;
        }
        let languages = languages_ref as CFArrayRef;
        let count = CFArrayGetCount(languages);
        for i in 0..count {
            let lang_ref = CFArrayGetValueAtIndex(languages, i) as CFStringRef;
            if lang_ref.is_null() {
                continue;
            }
            if cfstring_to_string(lang_ref).as_deref() == Some(language) {
                return true;
            }
        }
        false
    }
}

fn input_source_is_selectable(api: &TisApi, source: TISInputSourceRef) -> bool {
    unsafe {
        let selectable_ref =
            (api.get_property)(source, api.property_input_source_is_select_capable);
        !selectable_ref.is_null() && CFBooleanGetValue(selectable_ref as CFBooleanRef)
    }
}

/// Toggle between a Korean input source and a roman keyboard layout.
/// Inspects the currently selected source's language list; if it supports
/// Korean, switches to en via TISCopyInputSourceForLanguage. Korean direction
/// walks the enabled-source list and selects a source that declares `ko` and is
/// select-capable. This covers both Apple's input modes and third-party input
/// methods without modes.
fn toggle_korean_input_source() {
    let Some(api) = tis_api() else {
        log::warn!("TIS: API unavailable");
        return;
    };
    unsafe {
        let current = (api.copy_current)();
        if current.is_null() {
            log::warn!("TIS: failed to get current input source");
            return;
        }
        let current_is_korean = input_source_has_language(api, current, "ko");
        CFRelease(current);

        let want_korean = !current_is_korean;

        if !want_korean {
            // English direction: language lookup returns the keyboard layout
            // (e.g. com.apple.keylayout.ABC), which is directly selectable.
            let cf_lang = cfstring_from("en");
            if cf_lang.is_null() {
                log::warn!("TIS: CFStringCreateWithCString(en) failed");
                return;
            }
            let target = (api.copy_for_language)(cf_lang);
            CFRelease(cf_lang);
            if target.is_null() {
                log::warn!("TIS: no input source for language en");
                return;
            }
            let target_id = input_source_id(api, target).unwrap_or_default();
            let status = (api.select)(target);
            CFRelease(target);
            if status == 0 {
                log::info!("TIS: switched to {target_id} (en)");
            } else {
                log::warn!("TIS: select {target_id} failed (OSStatus {status})");
            }
            return;
        }

        // Korean direction: walk the enabled input-source list and pick the
        // first selectable source that declares Korean support.
        let Some(create_list) = api.create_list else {
            log::warn!("TIS: input-source list unavailable; cannot select Korean");
            return;
        };
        let list = create_list(std::ptr::null(), false);
        if list.is_null() {
            log::warn!("TIS: failed to enumerate input sources");
            return;
        }
        let count = CFArrayGetCount(list);
        let mut selected = false;
        for i in 0..count {
            let source = CFArrayGetValueAtIndex(list, i) as TISInputSourceRef;
            if input_source_has_language(api, source, "ko")
                && input_source_is_selectable(api, source)
            {
                let id = input_source_id(api, source).unwrap_or_else(|| "<unknown>".to_string());
                let status = (api.select)(source);
                if status == 0 {
                    log::info!("TIS: switched to {id}");
                    selected = true;
                    break;
                } else {
                    log::warn!("TIS: select {id} failed (OSStatus {status})");
                }
            }
        }
        CFRelease(list);
        if !selected {
            log::warn!("TIS: no selectable Korean input mode found");
        }
    }
}

fn key_event(event_source: CGEventSource, key: u16, state: u8, modifiers: XMods) {
    let event = match CGEvent::new_keyboard_event(event_source, key, state != 0) {
        Ok(e) => e,
        Err(_) => {
            log::warn!("unable to create key event");
            return;
        }
    };
    event.set_flags(to_cgevent_flags(modifiers));
    event.post(CGEventTapLocation::HID);
    log::trace!("key event: {key} {state}");
}

fn modifier_event(event_source: CGEventSource, depressed: XMods) {
    let Ok(event) = CGEvent::new(event_source) else {
        log::warn!("could not create CGEvent");
        return;
    };
    let flags = to_cgevent_flags(depressed);
    event.set_type(CGEventType::FlagsChanged);
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    log::trace!("modifiers updated: {depressed:?}");
}

fn get_display_at_point(x: CGFloat, y: CGFloat) -> Option<CGDirectDisplayID> {
    let mut displays: [CGDirectDisplayID; 16] = [0; 16];
    let mut display_count: u32 = 0;
    let rect = CGRect::new(&CGPoint::new(x, y), &CGSize::new(0.0, 0.0));

    let error = unsafe {
        CGGetDisplaysWithRect(
            rect,
            1,
            displays.as_mut_ptr(),
            &mut display_count as *mut u32,
        )
    };

    if error != 0 {
        log::warn!("error getting displays at point ({x}, {y}): {error}");
        return Option::None;
    }

    if display_count == 0 {
        log::debug!("no displays found at point ({x}, {y})");
        return Option::None;
    }

    displays.first().copied()
}

fn get_display_bounds(display: CGDirectDisplayID) -> (CGFloat, CGFloat, CGFloat, CGFloat) {
    unsafe {
        let bounds = CGDisplayBounds(display);
        let min_x = bounds.origin.x;
        let max_x = bounds.origin.x + bounds.size.width;
        let min_y = bounds.origin.y;
        let max_y = bounds.origin.y + bounds.size.height;
        (min_x as f64, min_y as f64, max_x as f64, max_y as f64)
    }
}

fn clamp_to_screen_space(
    current_x: CGFloat,
    current_y: CGFloat,
    dx: CGFloat,
    dy: CGFloat,
) -> (CGFloat, CGFloat) {
    // Check which display the mouse is currently on
    // Determine what the location of the mouse would be after applying the move
    // Get the display at the new location
    // If the point is not on a display
    //   Clamp the mouse to the current display
    // Else If the point is on a display
    //   Clamp the mouse to the new display
    let current_display = match get_display_at_point(current_x, current_y) {
        Some(display) => display,
        None => {
            log::warn!("could not get current display!");
            return (current_x, current_y);
        }
    };

    let new_x = current_x + dx;
    let new_y = current_y + dy;

    let final_display = get_display_at_point(new_x, new_y).unwrap_or(current_display);
    let (min_x, min_y, max_x, max_y) = get_display_bounds(final_display);

    (
        new_x.clamp(min_x, max_x - 1.),
        new_y.clamp(min_y, max_y - 1.),
    )
}

#[async_trait]
impl Emulation for MacOSEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        log::trace!("{event:?}");
        match event {
            Event::Pointer(pointer_event) => {
                match pointer_event {
                    PointerEvent::Motion { time: _, dx, dy } => {
                        let dx = dx * MOUSE_SPEED_MULTIPLIER;
                        let dy = dy * MOUSE_SPEED_MULTIPLIER;
                        let mut mouse_location = match self.get_mouse_location() {
                            Some(l) => l,
                            None => {
                                log::warn!("could not get mouse location!");
                                return Ok(());
                            }
                        };

                        let (new_mouse_x, new_mouse_y) =
                            clamp_to_screen_space(mouse_location.x, mouse_location.y, dx, dy);

                        mouse_location.x = new_mouse_x;
                        mouse_location.y = new_mouse_y;

                        // If any button is held, emit a drag event for it;
                        // otherwise emit a normal mouse-moved event.
                        let event_type = self
                            .pressed_buttons
                            .iter()
                            .next()
                            .map(|&btn| drag_event_type(btn))
                            .unwrap_or(CGEventType::MouseMoved);
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            mouse_location,
                            CGMouseButton::Left,
                        ) {
                            Ok(e) => e,
                            Err(_) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::Button {
                        time: _,
                        button,
                        state,
                    } => {
                        // Route mouse4 (BTN_BACK) / mouse5 (BTN_FORWARD) to
                        // F9 (Mission Control) / F11 (Show Desktop) unless the
                        // frontmost app is a browser/Finder, where back-forward
                        // is the natural behavior.
                        if matches!(button, BTN_BACK | BTN_FORWARD) {
                            if state == 1 {
                                let owner = frontmost_window_owner_name();
                                match back_forward_route(owner.as_deref()) {
                                    BackForwardRoute::Passthrough => {
                                        // fall through to existing OtherMouseDown logic
                                    }
                                    BackForwardRoute::CmdBracket => {
                                        let bracket_key: u16 = if button == BTN_BACK {
                                            0x21 // "["
                                        } else {
                                            0x1E // "]"
                                        };
                                        log::debug!(
                                            "mouse{} -> ⌘+{} (frontmost: {:?})",
                                            if button == BTN_BACK { 4 } else { 5 },
                                            if button == BTN_BACK { "[" } else { "]" },
                                            owner
                                        );
                                        send_cmd_key(
                                            self.event_source.clone(),
                                            bracket_key,
                                            self.modifier_state.get(),
                                        );
                                        self.synth_keyed_buttons.insert(button);
                                        return Ok(());
                                    }
                                    BackForwardRoute::SystemShortcut => {
                                        log::debug!(
                                            "mouse{} -> {} (frontmost: {:?})",
                                            if button == BTN_BACK { 4 } else { 5 },
                                            if button == BTN_BACK {
                                                "Mission Control"
                                            } else {
                                                "Show Desktop"
                                            },
                                            owner
                                        );
                                        if button == BTN_BACK {
                                            trigger_mission_control();
                                        } else {
                                            trigger_show_desktop();
                                        }
                                        self.synth_keyed_buttons.insert(button);
                                        return Ok(());
                                    }
                                }
                            } else if self.synth_keyed_buttons.remove(&button) {
                                // matching release for a synth-routed press
                                return Ok(());
                            }
                        }
                        // button number for OtherMouse events (3 = back, 4 = forward, etc.)
                        let cg_button_number: Option<i64> = match button {
                            BTN_BACK => Some(3),
                            BTN_FORWARD => Some(4),
                            _ => None,
                        };
                        let (event_type, mouse_button) = match (button, state) {
                            (BTN_LEFT, 1) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                            (BTN_LEFT, 0) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                            (BTN_RIGHT, 1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                            (BTN_RIGHT, 0) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                            (BTN_MIDDLE, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                            (BTN_MIDDLE, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                            (BTN_BACK, 1) | (BTN_FORWARD, 1) => {
                                (CGEventType::OtherMouseDown, CGMouseButton::Center)
                            }
                            (BTN_BACK, 0) | (BTN_FORWARD, 0) => {
                                (CGEventType::OtherMouseUp, CGMouseButton::Center)
                            }
                            _ => {
                                log::warn!("invalid button event: {button},{state}");
                                return Ok(());
                            }
                        };
                        // store button state using the evdev button code so
                        // back, forward, and middle are tracked independently
                        if state == 1 {
                            self.pressed_buttons.insert(button);
                        } else {
                            self.pressed_buttons.remove(&button);
                        }

                        // update double-click tracking using the evdev button
                        // code so that back/forward don't alias with middle
                        if state == 1 {
                            if self.previous_button == Some(button)
                                && self
                                    .previous_button_click
                                    .is_some_and(|i| i.elapsed() < DOUBLE_CLICK_INTERVAL)
                            {
                                self.button_click_state += 1;
                            } else {
                                self.button_click_state = 1;
                            }
                            self.previous_button = Some(button);
                            self.previous_button_click = Some(Instant::now());
                        }

                        log::debug!("click_state: {}", self.button_click_state);
                        let location = self.get_mouse_location().unwrap();
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            location,
                            mouse_button,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(
                            EventField::MOUSE_EVENT_CLICK_STATE,
                            self.button_click_state,
                        );
                        // Set the button number for extra buttons (back=3, forward=4)
                        if let Some(btn_num) = cg_button_number {
                            event.set_integer_value_field(
                                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                                btn_num,
                            );
                        }
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::Axis {
                        time: _,
                        axis,
                        value,
                    } => {
                        let value = value as i32;
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value, 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value, 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::PIXEL,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        const LINES_PER_STEP: i32 = 3;
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value / (120 / LINES_PER_STEP), 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value / (120 / LINES_PER_STEP), 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::LINE,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        event.post(CGEventTapLocation::HID);
                    }
                }

                // reset button click state in case it's not a button event
                if !matches!(pointer_event, PointerEvent::Button { .. }) {
                    self.button_click_state = 0;
                }
            }
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    // Korean IME remap: Right Alt is commonly repurposed as the
                    // Hangul/English toggle on Windows keyboards used with macOS.
                    // Synthetic input-source shortcuts are rejected by macOS, so
                    // toggle the input source via the Text Input Source API on
                    // press. Right Alt's normal modifier handling is also skipped
                    // so we don't emit an Option modifier event.
                    let remap_to_ime_toggle = key == 100;
                    if remap_to_ime_toggle {
                        log::debug!("Right Alt -> Korean IME toggle (state={state})");
                        if state == 1 && self.accept_ime_toggle() {
                            toggle_korean_input_source();
                        }
                        return Ok(());
                    }
                    // System-shortcut function keys: macOS rejects CGEvent-
                    // synthesized triggers for system shortcuts (the same
                    // policy that makes synthetic F18/Ctrl+Space fail for
                    // IME). Route F9 / F11 through the same trusted path
                    // mouse4/mouse5 use.
                    match key {
                        67 => {
                            // evdev KEY_F9 -> Mission Control
                            if state == 1 {
                                log::debug!("F9 -> Mission Control");
                                trigger_mission_control();
                            }
                            return Ok(());
                        }
                        87 => {
                            // evdev KEY_F11 -> Show Desktop
                            if state == 1 {
                                log::debug!("F11 -> Show Desktop");
                                trigger_show_desktop();
                            }
                            return Ok(());
                        }
                        // Media keys: macOS expects these as NSSystemDefined
                        // events, not regular keyboard events. A KeyDown at
                        // kVK_VolumeUp etc. would do nothing.
                        113 => {
                            // evdev KEY_MUTE — single state, no fine grain
                            if state == 1 {
                                log::debug!("Mute key -> NSSystemDefined MUTE");
                                post_media_key(NX_KEYTYPE_MUTE, false);
                            }
                            return Ok(());
                        }
                        114 => {
                            // evdev KEY_VOLUMEDOWN — fine step (¼ of native step)
                            if state == 1 {
                                log::debug!("VolumeDown -> NSSystemDefined SOUND_DOWN (fine)");
                                post_media_key(NX_KEYTYPE_SOUND_DOWN, true);
                            }
                            return Ok(());
                        }
                        115 => {
                            // evdev KEY_VOLUMEUP — fine step (¼ of native step)
                            if state == 1 {
                                log::debug!("VolumeUp -> NSSystemDefined SOUND_UP (fine)");
                                post_media_key(NX_KEYTYPE_SOUND_UP, true);
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                    let code = match KeyMap::from_key_mapping(KeyMapping::Evdev(key as u16)) {
                        Ok(k) => k.mac as CGKeyCode,
                        Err(_) => {
                            log::warn!("unable to map key event for evdev key {key}");
                            return Ok(());
                        }
                    };
                    log::trace!("key event: evdev={key} -> macos={code} (state={state})");
                    let is_modifier = update_modifiers(&self.modifier_state, key, state);
                    if is_modifier {
                        modifier_event(self.event_source.clone(), self.modifier_state.get());
                    }
                    match state {
                        // pressed
                        1 => self.spawn_repeat_task(code).await,
                        _ => self.cancel_repeat_task().await,
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    set_modifiers(&self.modifier_state, depressed, latched, locked, group);
                    modifier_event(self.event_source.clone(), self.modifier_state.get());
                }
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}

    fn display_bounds(&self) -> Option<(u32, u32)> {
        // Union of every active display's rectangle. Matches the
        // shape used on the input-capture side so the host's
        // wall-press model is consistent across both ends.
        let displays = CGDisplay::active_displays().ok()?;
        let mut xmin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymin = f64::INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for id in displays {
            let bounds = CGDisplay::new(id).bounds();
            xmin = xmin.min(bounds.origin.x);
            xmax = xmax.max(bounds.origin.x + bounds.size.width);
            ymin = ymin.min(bounds.origin.y);
            ymax = ymax.max(bounds.origin.y + bounds.size.height);
        }
        if xmax <= xmin || ymax <= ymin {
            return None;
        }
        Some(((xmax - xmin) as u32, (ymax - ymin) as u32))
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        let pt = CGPoint {
            x: x as CGFloat,
            y: y as CGFloat,
        };
        // CGDisplay::warp_mouse_cursor_position is a global Quartz
        // call; it doesn't matter which CGDisplay receiver we use.
        let _ = CGDisplay::warp_mouse_cursor_position(pt);
        Ok(())
    }
}

fn update_modifiers(modifiers: &Cell<XMods>, key: u32, state: u8) -> bool {
    if let Ok(key) = scancode::Linux::try_from(key) {
        let mask = match key {
            scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
            scancode::Linux::KeyCapsLock => XMods::LockMask,
            scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
            scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => XMods::Mod1Mask,
            scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
            _ => XMods::empty(),
        };
        // unchanged
        if mask.is_empty() {
            return false;
        }
        let mut mods = modifiers.get();
        match state {
            1 => mods.insert(mask),
            _ => mods.remove(mask),
        }
        modifiers.set(mods);
        true
    } else {
        false
    }
}

fn set_modifiers(
    active_modifiers: &Cell<XMods>,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) {
    let depressed = XMods::from_bits(depressed).unwrap_or_default();
    let _latched = XMods::from_bits(latched).unwrap_or_default();
    let _locked = XMods::from_bits(locked).unwrap_or_default();
    let _group = XMods::from_bits(group).unwrap_or_default();

    // we only care about the depressed modifiers for now
    active_modifiers.replace(depressed);
}

fn to_cgevent_flags(depressed: XMods) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if depressed.contains(XMods::ShiftMask) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if depressed.contains(XMods::LockMask) {
        flags |= CGEventFlags::CGEventFlagAlphaShift;
    }
    if depressed.contains(XMods::ControlMask) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if depressed.contains(XMods::Mod1Mask) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if depressed.contains(XMods::Mod4Mask) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}
