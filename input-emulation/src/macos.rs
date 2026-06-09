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
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowOwnerPID,
};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};
use keycode::{KeyMap, KeyMapping};
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{CStr, c_char, c_void};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{sync::Notify, task::JoinHandle};

use super::error::MacOSEmulationCreationError;

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// After this much idle time on the keyboard channel, treat any still-held
/// modifier or running key-repeat as stranded (a press/release lost over
/// unreliable UDP) and self-heal before processing the next key. Long enough
/// not to disturb active typing or a brief deliberate modifier hold; short
/// enough to clear a stuck Cmd/Ctrl before the user's next toggle press.
const STUCK_RESET_IDLE: Duration = Duration::from_millis(800);
/// Hard upper bound on auto-repeat cycles for a single held key, so a lost
/// key-up can never drive an unbounded repeat flood into the focused app
/// (~10s at DEFAULT_REPEAT_INTERVAL — far beyond any real key hold).
const MAX_KEY_REPEATS: u32 = 300;

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
    /// notify to cancel key repeats
    notify_repeat_task: Arc<Notify>,
    /// timestamp of the last processed keyboard event, used to detect an idle
    /// gap after which still-held state is likely stranded (a release lost
    /// over UDP) and should be self-healed before the next key.
    last_keyboard_event: Option<Instant>,
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
            last_keyboard_event: None,
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
                let mut repeats: u32 = 0;
                loop {
                    key_event(event_source.clone(), key, 1, modifiers.get());
                    repeats += 1;
                    if repeats >= MAX_KEY_REPEATS {
                        log::warn!(
                            "key {key} hit repeat safety limit ({MAX_KEY_REPEATS}); \
                             releasing — a key-up was likely lost"
                        );
                        break;
                    }
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
    fn CGEventPost(tap: u32, event: *const c_void);
}

// Runtime symbol lookup for Objective-C framework helpers.
unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, sym: *const std::ffi::c_char) -> *mut c_void;
}
const RTLD_LAZY: i32 = 1;
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

fn canonical_side_button(button: u32) -> Option<u32> {
    match button {
        BTN_BACK | 3 => Some(BTN_BACK),
        BTN_FORWARD | 4 => Some(BTN_FORWARD),
        _ => None,
    }
}

fn side_button_name(button: u32) -> &'static str {
    if button == BTN_BACK {
        "back"
    } else {
        "forward"
    }
}

fn side_button_cg_number(button: u32) -> Option<i64> {
    match canonical_side_button(button) {
        Some(BTN_BACK) => Some(3),
        Some(BTN_FORWARD) => Some(4),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SideButtonRoute {
    MissionControl,
    ShowDesktop,
    NativeMouse,
}

fn is_chrome_bundle_id(bundle_id: &str) -> bool {
    matches!(bundle_id, "com.google.Chrome" | "com.google.Chrome.canary")
}

fn side_button_route_for_bundle(
    button: u32,
    frontmost_bundle_id: Option<&str>,
) -> Option<SideButtonRoute> {
    let side_button = canonical_side_button(button)?;
    if frontmost_bundle_id.is_some_and(is_chrome_bundle_id) {
        return Some(SideButtonRoute::NativeMouse);
    }
    match side_button {
        BTN_BACK => Some(SideButtonRoute::MissionControl),
        BTN_FORWARD => Some(SideButtonRoute::ShowDesktop),
        _ => None,
    }
}

fn side_button_route(button: u32) -> Option<SideButtonRoute> {
    let frontmost_bundle_id = frontmost_bundle_identifier();
    side_button_route_for_bundle(button, frontmost_bundle_id.as_deref())
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

/// Triggers Show Desktop by posting F11 from a `HIDSystemState` CGEventSource,
/// the event-source state that sits closest to a real HID interrupt. The hope
/// is that WindowServer's mid-transition reverse handler (which lets native
/// F11 cancel an in-flight Show Desktop transition) accepts this source.
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

// ---- NSAutoreleasePool via Objective-C runtime + dlsym --------------------

type ObjcId = *const c_void;
type ObjcSel = *const c_void;
type ObjcClass = *const c_void;

type FnObjcGetClass = unsafe extern "C" fn(name: *const std::ffi::c_char) -> ObjcClass;
type FnSelRegisterName = unsafe extern "C" fn(name: *const std::ffi::c_char) -> ObjcSel;
type FnMsgSend0 = unsafe extern "C" fn(ObjcId, ObjcSel) -> ObjcId;

struct AutoreleasePoolApi {
    nsautoreleasepool_class: ObjcClass,
    alloc_sel: ObjcSel,
    init_sel: ObjcSel,
    drain_sel: ObjcSel,
    msg_send_0: FnMsgSend0,
}

unsafe impl Send for AutoreleasePoolApi {}
unsafe impl Sync for AutoreleasePoolApi {}

static AUTORELEASE_POOL_API: std::sync::OnceLock<Option<AutoreleasePoolApi>> =
    std::sync::OnceLock::new();

fn autorelease_pool_api() -> Option<&'static AutoreleasePoolApi> {
    AUTORELEASE_POOL_API
        .get_or_init(|| unsafe {
            let foundation_path = c"/System/Library/Frameworks/Foundation.framework/Foundation";
            let _foundation = dlopen(foundation_path.as_ptr(), RTLD_LAZY);

            let get_class = dlsym(RTLD_DEFAULT, c"objc_getClass".as_ptr());
            let sel_register = dlsym(RTLD_DEFAULT, c"sel_registerName".as_ptr());
            let msg_send = dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr());
            if get_class.is_null() || sel_register.is_null() || msg_send.is_null() {
                log::warn!("NSAutoreleasePool: objc runtime missing");
                return None;
            }
            let get_class: FnObjcGetClass = std::mem::transmute(get_class);
            let sel_register: FnSelRegisterName = std::mem::transmute(sel_register);
            let msg_send_0: FnMsgSend0 = std::mem::transmute(msg_send);

            let nsautoreleasepool_class = get_class(c"NSAutoreleasePool".as_ptr());
            if nsautoreleasepool_class.is_null() {
                log::warn!("NSAutoreleasePool: class not found");
                return None;
            }
            Some(AutoreleasePoolApi {
                nsautoreleasepool_class,
                alloc_sel: sel_register(c"alloc".as_ptr()),
                init_sel: sel_register(c"init".as_ptr()),
                drain_sel: sel_register(c"drain".as_ptr()),
                msg_send_0,
            })
        })
        .as_ref()
}

// ---- Frontmost application lookup ----------------------------------------
//
// The lan-mouse daemon has no AppKit run loop, so
// `NSWorkspace.frontmostApplication` is never refreshed by activation
// notifications: it stays frozen at whatever happened to be frontmost when the
// process first queried it. That made Chrome detection a coin flip depending on
// what was focused at daemon start. Instead we ask the WindowServer for the
// on-screen window list (a fresh query on every call, no run loop required) to
// find the frontmost regular window's owner pid, then resolve that pid to a
// bundle id via `NSRunningApplication`.

type FnMsgSendPid = unsafe extern "C" fn(ObjcId, ObjcSel, i32) -> ObjcId;
type FnMsgSendCString = unsafe extern "C" fn(ObjcId, ObjcSel) -> *const c_char;

struct FrontmostApplicationApi {
    running_application_class: ObjcClass,
    running_application_with_pid_sel: ObjcSel,
    bundle_identifier_sel: ObjcSel,
    utf8_string_sel: ObjcSel,
    msg_send_0: FnMsgSend0,
    msg_send_pid: FnMsgSendPid,
    msg_send_cstring: FnMsgSendCString,
}

unsafe impl Send for FrontmostApplicationApi {}
unsafe impl Sync for FrontmostApplicationApi {}

static FRONTMOST_APPLICATION_API: std::sync::OnceLock<Option<FrontmostApplicationApi>> =
    std::sync::OnceLock::new();

fn frontmost_application_api() -> Option<&'static FrontmostApplicationApi> {
    FRONTMOST_APPLICATION_API
        .get_or_init(|| unsafe {
            let _appkit = dlopen(
                c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(),
                RTLD_LAZY,
            );
            let get_class = dlsym(RTLD_DEFAULT, c"objc_getClass".as_ptr());
            let sel_register = dlsym(RTLD_DEFAULT, c"sel_registerName".as_ptr());
            let msg_send = dlsym(RTLD_DEFAULT, c"objc_msgSend".as_ptr());
            if get_class.is_null() || sel_register.is_null() || msg_send.is_null() {
                log::warn!("frontmost-app: objc runtime missing");
                return None;
            }
            let get_class: FnObjcGetClass = std::mem::transmute(get_class);
            let sel_register: FnSelRegisterName = std::mem::transmute(sel_register);
            let msg_send_0: FnMsgSend0 = std::mem::transmute(msg_send);
            let msg_send_pid: FnMsgSendPid = std::mem::transmute(msg_send);
            let msg_send_cstring: FnMsgSendCString = std::mem::transmute(msg_send);

            let running_application_class = get_class(c"NSRunningApplication".as_ptr());
            if running_application_class.is_null() {
                log::warn!("frontmost-app: NSRunningApplication class not found");
                return None;
            }
            Some(FrontmostApplicationApi {
                running_application_class,
                running_application_with_pid_sel: sel_register(
                    c"runningApplicationWithProcessIdentifier:".as_ptr(),
                ),
                bundle_identifier_sel: sel_register(c"bundleIdentifier".as_ptr()),
                utf8_string_sel: sel_register(c"UTF8String".as_ptr()),
                msg_send_0,
                msg_send_pid,
                msg_send_cstring,
            })
        })
        .as_ref()
}

/// Returns the pid that owns the frontmost on-screen regular window, queried
/// fresh from the WindowServer (no AppKit run loop required). Reading only the
/// owner pid and window layer needs no Screen Recording permission.
fn frontmost_window_pid() -> Option<i64> {
    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let window_list = copy_window_info(options, kCGNullWindowID)?;
    let layer_key = unsafe { CFString::wrap_under_get_rule(kCGWindowLayer) };
    let owner_pid_key = unsafe { CFString::wrap_under_get_rule(kCGWindowOwnerPID) };
    // Windows come back front-to-back; the first one at the normal window layer
    // (0) belongs to the frontmost regular application (menu bar, dock, status
    // items and the like sit on higher layers).
    for window in window_list.iter() {
        let dict_ref = (*window) as CFDictionaryRef;
        if dict_ref.is_null() {
            continue;
        }
        let dict = unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(dict_ref) };
        let layer = dict
            .find(&layer_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64());
        if layer != Some(0) {
            continue;
        }
        if let Some(pid) = dict
            .find(&owner_pid_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
        {
            return Some(pid);
        }
    }
    None
}

fn frontmost_bundle_identifier() -> Option<String> {
    let pid = frontmost_window_pid()?;
    let api = frontmost_application_api()?;
    let pool_api = autorelease_pool_api();
    unsafe {
        let pool = pool_api.map(|p| {
            let alloc = (p.msg_send_0)(p.nsautoreleasepool_class, p.alloc_sel);
            if alloc.is_null() {
                std::ptr::null()
            } else {
                (p.msg_send_0)(alloc, p.init_sel)
            }
        });

        // +[NSRunningApplication runningApplicationWithProcessIdentifier:] does a
        // live LaunchServices lookup, so it does not depend on a run loop the way
        // NSWorkspace's cached frontmostApplication does.
        let app = (api.msg_send_pid)(
            api.running_application_class,
            api.running_application_with_pid_sel,
            pid as i32,
        );
        let result = if app.is_null() {
            None
        } else {
            let bundle_id = (api.msg_send_0)(app, api.bundle_identifier_sel);
            if bundle_id.is_null() {
                None
            } else {
                let raw = (api.msg_send_cstring)(bundle_id, api.utf8_string_sel);
                if raw.is_null() {
                    None
                } else {
                    CStr::from_ptr(raw).to_str().ok().map(ToOwned::to_owned)
                }
            }
        };

        if let (Some(p), Some(pool_obj)) = (pool_api, pool) {
            if !pool_obj.is_null() {
                let _ = (p.msg_send_0)(pool_obj, p.drain_sel);
            }
        }
        result
    }
}

// ---- NSEvent media-key synthesis via Objective-C runtime + dlsym ----------
//
// macOS routes hardware volume / mute keys as NSSystemDefined events, not as
// regular keyboard events. Posting CGKeyDown/CGKeyUp at kVK_VolumeUp etc. does
// not change volume, so synthesize the same event shape used by hardware keys.

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

fn post_media_key(key_type: u32, fine_step: bool) {
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
    let pool_api = autorelease_pool_api();
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

fn macos_keycode_from_evdev(key: u32) -> Option<CGKeyCode> {
    KeyMap::from_key_mapping(KeyMapping::Evdev(key as u16))
        .ok()
        .map(|k| k.mac as CGKeyCode)
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

fn modifier_event(event_source: CGEventSource, depressed: XMods, key: Option<CGKeyCode>) {
    let Ok(event) = CGEvent::new(event_source) else {
        log::warn!("could not create CGEvent");
        return;
    };
    let flags = to_cgevent_flags(depressed);
    event.set_type(CGEventType::FlagsChanged);
    event.set_flags(flags);
    if let Some(key) = key {
        event.set_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE, key as i64);
    }
    event.post(CGEventTapLocation::HID);
    log::trace!("modifiers updated: key={key:?} depressed={depressed:?}");
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

fn valid_display_bounds(
    display: CGDirectDisplayID,
) -> Option<(CGFloat, CGFloat, CGFloat, CGFloat)> {
    let (min_x, min_y, max_x, max_y) = get_display_bounds(display);
    if max_x <= min_x || max_y <= min_y {
        log::warn!(
            "ignoring invalid display bounds for display {display}: ({min_x}, {min_y})-({max_x}, {max_y})"
        );
        return None;
    }
    Some((min_x, min_y, max_x, max_y))
}

/// Union of all active displays as `(origin_x, origin_y, width, height)`
/// in the global Quartz coordinate system anchored at the MAIN
/// display's top-left. The origin is negative on an axis when a
/// display sits left of / above the main one.
fn active_display_rect() -> Option<(CGFloat, CGFloat, CGFloat, CGFloat)> {
    let displays = CGDisplay::active_displays().ok()?;
    let mut xmin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for id in displays {
        let Some((min_x, min_y, max_x, max_y)) = valid_display_bounds(id) else {
            continue;
        };
        xmin = xmin.min(min_x);
        xmax = xmax.max(max_x);
        ymin = ymin.min(min_y);
        ymax = ymax.max(max_y);
    }
    if xmax <= xmin || ymax <= ymin {
        return None;
    }
    Some((xmin, ymin, xmax - xmin, ymax - ymin))
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
            log::debug!("could not get current display");
            return (current_x, current_y);
        }
    };

    let new_x = current_x + dx;
    let new_y = current_y + dy;

    let final_display = get_display_at_point(new_x, new_y).unwrap_or(current_display);
    let Some((min_x, min_y, max_x, max_y)) =
        valid_display_bounds(final_display).or_else(|| valid_display_bounds(current_display))
    else {
        return (current_x, current_y);
    };

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
                        // Route side buttons to native browser back/forward
                        // only for Chrome. Everywhere else keeps the existing
                        // F9/F11 behavior. Accept both lan-mouse's evdev
                        // BTN_BACK/FORWARD constants and raw macOS OtherMouse
                        // button numbers 3/4.
                        if let Some(side_button) = canonical_side_button(button) {
                            if state != 1 {
                                if self.synth_keyed_buttons.remove(&side_button) {
                                    // matching release for a synth-routed press
                                    return Ok(());
                                }
                            } else {
                                match side_button_route(button) {
                                    Some(SideButtonRoute::NativeMouse) => {
                                        log::info!(
                                            "side mouse button {} (raw={button}) -> native mouse event for Chrome",
                                            side_button_name(side_button)
                                        );
                                    }
                                    Some(SideButtonRoute::MissionControl) => {
                                        log::info!(
                                            "side mouse button {} (raw={button}) -> F9 / Mission Control",
                                            side_button_name(side_button)
                                        );
                                        trigger_mission_control();
                                        self.synth_keyed_buttons.insert(side_button);
                                        return Ok(());
                                    }
                                    Some(SideButtonRoute::ShowDesktop) => {
                                        log::info!(
                                            "side mouse button {} (raw={button}) -> F11 / Show Desktop",
                                            side_button_name(side_button)
                                        );
                                        trigger_show_desktop();
                                        self.synth_keyed_buttons.insert(side_button);
                                        return Ok(());
                                    }
                                    None => {}
                                }
                            }
                        }
                        // button number for OtherMouse events (3 = back, 4 = forward, etc.)
                        let cg_button_number = side_button_cg_number(button);
                        let (event_type, mouse_button) = match (button, state) {
                            (BTN_LEFT, 1) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                            (BTN_LEFT, 0) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                            (BTN_RIGHT, 1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                            (BTN_RIGHT, 0) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                            (BTN_MIDDLE, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                            (BTN_MIDDLE, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                            (_, 1) if canonical_side_button(button).is_some() => {
                                (CGEventType::OtherMouseDown, CGMouseButton::Center)
                            }
                            (_, 0) if canonical_side_button(button).is_some() => {
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
                        let Some(location) = self.get_mouse_location() else {
                            log::warn!("button event skipped: mouse location unavailable");
                            return Ok(());
                        };
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
                    // Self-heal state stranded by a release lost over UDP. The
                    // guest tracks modifiers incrementally, so a dropped
                    // key-up leaves a modifier stuck in `modifier_state`,
                    // injected into every later event's flags — which makes a
                    // downstream IME see 2 modifiers and abort its bare
                    // right-Option toggle. After an idle gap on the keyboard
                    // channel, cancel any runaway key-repeat, and — only when
                    // this event is itself a modifier *press* (e.g. the toggle
                    // key) — drop the stale modifier set first so that press
                    // is evaluated clean. Restricting the clear to modifier
                    // presses preserves a legitimately-held modifier across a
                    // pause followed by a normal keystroke. With the Windows
                    // capture's absolute `Modifiers` resync deployed, the
                    // resync clears the staleness first and refreshes the idle
                    // clock, so this path is only a backstop for a dropped
                    // resync.
                    let now = Instant::now();
                    let idle = self
                        .last_keyboard_event
                        .is_some_and(|t| now.duration_since(t) >= STUCK_RESET_IDLE);
                    self.last_keyboard_event = Some(now);
                    if idle {
                        self.cancel_repeat_task().await;
                        if state == 1
                            && evdev_is_modifier(key)
                            && !self.modifier_state.get().is_empty()
                        {
                            self.modifier_state.set(XMods::empty());
                            modifier_event(self.event_source.clone(), XMods::empty(), None);
                        }
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
                        113 => {
                            // evdev KEY_MUTE
                            if state == 1 {
                                log::debug!("Mute key -> NSSystemDefined MUTE");
                                post_media_key(NX_KEYTYPE_MUTE, false);
                            }
                            return Ok(());
                        }
                        114 => {
                            // evdev KEY_VOLUMEDOWN, with fine-step modifiers.
                            if state == 1 {
                                log::debug!("VolumeDown -> NSSystemDefined SOUND_DOWN (fine)");
                                post_media_key(NX_KEYTYPE_SOUND_DOWN, true);
                            }
                            return Ok(());
                        }
                        115 => {
                            // evdev KEY_VOLUMEUP, with fine-step modifiers.
                            if state == 1 {
                                log::debug!("VolumeUp -> NSSystemDefined SOUND_UP (fine)");
                                post_media_key(NX_KEYTYPE_SOUND_UP, true);
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                    let code = match macos_keycode_from_evdev(key) {
                        Some(code) => code,
                        None => {
                            log::warn!("unable to map key event for evdev key {key}");
                            return Ok(());
                        }
                    };
                    log::trace!("key event: evdev={key} -> macos={code} (state={state})");
                    let is_modifier = update_modifiers(&self.modifier_state, key, state);
                    if is_modifier {
                        modifier_event(
                            self.event_source.clone(),
                            self.modifier_state.get(),
                            Some(code),
                        );
                        return Ok(());
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
                    modifier_event(self.event_source.clone(), self.modifier_state.get(), None);
                    // This absolute resync is authoritative; refresh the idle
                    // clock so an immediately-following `Key` (e.g. the Windows
                    // capture sends `Modifiers` right before each modifier key)
                    // does not re-trigger the Key-arm idle self-heal and undo
                    // a modifier this resync just correctly set.
                    self.last_keyboard_event = Some(Instant::now());
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
        active_display_rect().map(|(_, _, width, height)| (width as u32, height as u32))
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        // `x`/`y` arrive as 0-based virtual coordinates (the receiver
        // scales the host's normalized fraction against the size from
        // `display_bounds`, the display-union extent). The global Quartz
        // coordinate system is anchored at the MAIN display's top-left,
        // so a display left of / above main occupies negative coords.
        // Offset by the union origin so the point lands on the true
        // union edge rather than main's edge — a no-op (0, 0) for a
        // single-display / main-at-origin layout, the fix for a left/top
        // secondary. Mirrors the input-capture origin handling and the
        // Windows backend.
        let (ox, oy) = active_display_rect()
            .map(|(origin_x, origin_y, _, _)| (origin_x, origin_y))
            .unwrap_or((0., 0.));
        let pt = CGPoint {
            x: x as CGFloat + ox,
            y: y as CGFloat + oy,
        };
        // CGDisplay::warp_mouse_cursor_position is a global Quartz
        // call; it doesn't matter which CGDisplay receiver we use.
        let _ = CGDisplay::warp_mouse_cursor_position(pt);
        Ok(())
    }
}

/// Whether an evdev key code is a modifier key — mirrors the set that
/// `update_modifiers` maps to a non-empty `XMods` mask.
fn evdev_is_modifier(key: u32) -> bool {
    matches!(
        scancode::Linux::try_from(key),
        Ok(scancode::Linux::KeyLeftShift
            | scancode::Linux::KeyRightShift
            | scancode::Linux::KeyCapsLock
            | scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftAlt
            | scancode::Linux::KeyRightalt
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta)
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_routes_side_buttons_as_native_mouse_events() {
        assert_eq!(
            side_button_route_for_bundle(BTN_BACK, Some("com.google.Chrome")),
            Some(SideButtonRoute::NativeMouse)
        );
        assert_eq!(
            side_button_route_for_bundle(BTN_FORWARD, Some("com.google.Chrome")),
            Some(SideButtonRoute::NativeMouse)
        );
        assert_eq!(
            side_button_route_for_bundle(3, Some("com.google.Chrome")),
            Some(SideButtonRoute::NativeMouse)
        );
        assert_eq!(
            side_button_route_for_bundle(4, Some("com.google.Chrome")),
            Some(SideButtonRoute::NativeMouse)
        );
        assert_eq!(
            side_button_route_for_bundle(BTN_BACK, Some("com.google.Chrome.canary")),
            Some(SideButtonRoute::NativeMouse)
        );
    }

    #[test]
    fn non_chrome_routes_side_buttons_to_existing_function_keys() {
        assert_eq!(
            side_button_route_for_bundle(BTN_BACK, Some("com.apple.finder")),
            Some(SideButtonRoute::MissionControl)
        );
        assert_eq!(
            side_button_route_for_bundle(BTN_FORWARD, Some("com.apple.finder")),
            Some(SideButtonRoute::ShowDesktop)
        );
        assert_eq!(
            side_button_route_for_bundle(3, None),
            Some(SideButtonRoute::MissionControl)
        );
        assert_eq!(
            side_button_route_for_bundle(4, None),
            Some(SideButtonRoute::ShowDesktop)
        );
    }

    #[test]
    fn non_side_buttons_do_not_use_side_button_routing() {
        assert_eq!(
            side_button_route_for_bundle(BTN_LEFT, Some("com.google.Chrome")),
            None
        );
    }

    #[test]
    fn right_alt_evdev_maps_to_right_option_keycode() {
        const MACOS_RIGHT_OPTION: CGKeyCode = 0x3d;

        assert_eq!(
            macos_keycode_from_evdev(scancode::Linux::KeyRightalt as u32),
            Some(MACOS_RIGHT_OPTION)
        );
    }

    #[test]
    fn evdev_is_modifier_matches_modifier_keys_only() {
        // Modifier keys (the set update_modifiers maps to an XMods bit) gate
        // the idle self-heal's modifier clear.
        for key in [
            scancode::Linux::KeyLeftShift,
            scancode::Linux::KeyLeftCtrl,
            scancode::Linux::KeyRightalt,
            scancode::Linux::KeyLeftMeta,
        ] {
            assert!(evdev_is_modifier(key as u32), "{key:?} should be a modifier");
        }
        for key in [
            scancode::Linux::KeyA,
            scancode::Linux::Key1,
            scancode::Linux::KeySpace,
        ] {
            assert!(
                !evdev_is_modifier(key as u32),
                "{key:?} should not be a modifier"
            );
        }
        // an out-of-range evdev code is not a modifier
        assert!(!evdev_is_modifier(u32::MAX));
    }
}
