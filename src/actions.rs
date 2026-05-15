use clap::Args;
use lan_mouse_ipc::{ActionTrigger, ClientAction};
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ActionError {
    #[error("DDC/VCP actions are only supported on Windows")]
    Unsupported,
    #[error("blocking action task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Platform(String),
}

pub(crate) async fn run(action: ClientAction) -> Result<(), ActionError> {
    match action {
        ClientAction::DdcVcp {
            monitor,
            code,
            value,
            ..
        } => tokio::task::spawn_blocking(move || platform::set_vcp(monitor, code, value)).await?,
    }
}

pub(crate) fn is_fast_enter_prefire_supported(action: &ClientAction) -> bool {
    matches!(
        action,
        ClientAction::DdcVcp {
            on: ActionTrigger::Enter,
            ..
        }
    ) && platform::supports_ddc_vcp()
}

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct TestDdcArgs {
    /// monitor selector: index, display name, hardware ID, or description
    #[arg(long)]
    monitor: Option<String>,

    /// VCP code; 0x60 is input source
    #[arg(long, default_value_t = 0x60)]
    code: u8,

    /// VCP value to write
    #[arg(long)]
    value: u32,
}

pub async fn test_ddc(args: TestDdcArgs) -> Result<(), ActionError> {
    run(ClientAction::DdcVcp {
        on: lan_mouse_ipc::ActionTrigger::Enter,
        monitor: args.monitor,
        code: args.code,
        value: args.value,
    })
    .await
}

#[cfg(windows)]
mod platform {
    use super::ActionError;
    use std::{mem, ptr::addr_of_mut};
    use windows::Win32::Devices::Display::{
        DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
        GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetVCPFeature,
    };
    use windows::Win32::Foundation::{FALSE, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
        MONITORINFO, MONITORINFOEXW,
    };
    use windows::core::{BOOL, Error as WindowsError, PCWSTR};

    struct PhysicalMonitor {
        index: usize,
        raw: PHYSICAL_MONITOR,
        description: String,
        display_name: String,
        display_string: String,
        monitor_id: String,
        monitor_string: String,
    }

    pub(super) fn set_vcp(
        monitor_selector: Option<String>,
        code: u8,
        value: u32,
    ) -> Result<(), ActionError> {
        let monitors = enumerate_physical_monitors()?;
        let Some(monitor) = select_monitor(&monitors, monitor_selector.as_deref()) else {
            let available = monitors
                .iter()
                .map(|m| {
                    format!(
                        "{}:{} display={} monitor_id={}",
                        m.index, m.description, m.display_name, m.monitor_id
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ActionError::Platform(format!(
                "no matching physical monitor for selector {:?}; available: [{}]",
                monitor_selector, available
            )));
        };
        let ok = unsafe { SetVCPFeature(monitor.raw.hPhysicalMonitor, code, value) };
        if ok == 0 {
            Err(ActionError::Platform(format!(
                "SetVCPFeature(code=0x{code:02x}, value={value}) failed on monitor {index}:{desc}: {err}",
                index = monitor.index,
                desc = monitor.description,
                err = WindowsError::from_win32(),
            )))
        } else {
            log::info!(
                "ddc_vcp action set code=0x{code:02x} value={value} on monitor {index}:{desc}",
                index = monitor.index,
                desc = monitor.description,
            );
            Ok(())
        }
    }

    fn select_monitor<'a>(
        monitors: &'a [PhysicalMonitor],
        selector: Option<&str>,
    ) -> Option<&'a PhysicalMonitor> {
        let selector = selector.map(str::trim).filter(|s| !s.is_empty());
        let Some(selector) = selector else {
            return monitors.first();
        };
        if let Ok(index) = selector.parse::<usize>() {
            return monitors.iter().find(|m| m.index == index);
        }
        let selector = selector.to_ascii_lowercase();
        monitors.iter().find(|monitor| {
            monitor
                .selector_labels()
                .iter()
                .any(|label| label.contains(&selector))
        })
    }

    fn enumerate_physical_monitors() -> Result<Vec<PhysicalMonitor>, ActionError> {
        let mut logical = Vec::<HMONITOR>::new();
        unsafe extern "system" fn callback(
            monitor: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
            monitors.push(monitor);
            BOOL(1)
        }
        let ok = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(callback),
                LPARAM(addr_of_mut!(logical) as isize),
            )
        };
        if !ok.as_bool() {
            return Err(ActionError::Platform(format!(
                "EnumDisplayMonitors failed: {}",
                WindowsError::from_win32()
            )));
        }

        let mut out = Vec::new();
        for monitor in logical {
            let display = display_metadata(monitor)?;
            let mut count = 0;
            unsafe {
                GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count)
                    .map_err(|e| ActionError::Platform(e.to_string()))?;
            }
            if count == 0 {
                continue;
            }
            let mut physical = vec![PHYSICAL_MONITOR::default(); count as usize];
            unsafe {
                GetPhysicalMonitorsFromHMONITOR(monitor, &mut physical)
                    .map_err(|e| ActionError::Platform(e.to_string()))?;
            }
            for raw in physical {
                let index = out.len();
                let description = monitor_description(&raw);
                out.push(PhysicalMonitor {
                    index,
                    raw,
                    description,
                    display_name: display.display_name.clone(),
                    display_string: display.display_string.clone(),
                    monitor_id: display.monitor_id.clone(),
                    monitor_string: display.monitor_string.clone(),
                });
            }
        }
        if out.is_empty() {
            return Err(ActionError::Platform(
                "no physical monitors were enumerated".to_owned(),
            ));
        }
        Ok(out)
    }

    struct DisplayMetadata {
        display_name: String,
        display_string: String,
        monitor_id: String,
        monitor_string: String,
    }

    fn display_metadata(monitor: HMONITOR) -> Result<DisplayMetadata, ActionError> {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
        let ok = unsafe {
            GetMonitorInfoW(
                monitor,
                &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
            )
        };
        if !ok.as_bool() {
            return Err(ActionError::Platform(format!(
                "GetMonitorInfoW failed: {}",
                WindowsError::from_win32()
            )));
        }

        let display_name = wide_to_string(&info.szDevice);
        let mut display_device = DISPLAY_DEVICEW::default();
        display_device.cb = mem::size_of::<DISPLAY_DEVICEW>() as u32;
        let display_ok = unsafe {
            EnumDisplayDevicesW(
                PCWSTR::from_raw(info.szDevice.as_ptr()),
                0,
                &mut display_device,
                0,
            )
        };

        if display_ok == FALSE {
            return Ok(DisplayMetadata {
                display_name,
                display_string: String::new(),
                monitor_id: String::new(),
                monitor_string: String::new(),
            });
        }

        Ok(DisplayMetadata {
            display_name,
            display_string: wide_to_string(&display_device.DeviceString),
            monitor_id: wide_to_string(&display_device.DeviceID),
            monitor_string: wide_to_string(&display_device.DeviceName),
        })
    }

    fn monitor_description(monitor: &PHYSICAL_MONITOR) -> String {
        let description = monitor.szPhysicalMonitorDescription;
        wide_to_string(&description)
    }

    fn wide_to_string(description: &[u16]) -> String {
        let len = description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(description.len());
        String::from_utf16_lossy(&description[..len])
    }

    impl PhysicalMonitor {
        fn selector_labels(&self) -> Vec<String> {
            [
                self.index.to_string(),
                format!("monitor{}", self.index),
                self.description.clone(),
                self.display_name.clone(),
                self.display_string.clone(),
                self.monitor_id.clone(),
                self.monitor_string.clone(),
            ]
            .into_iter()
            .filter(|label| !label.is_empty())
            .map(|label| label.to_ascii_lowercase())
            .collect()
        }
    }

    impl Drop for PhysicalMonitor {
        fn drop(&mut self) {
            let raw = [self.raw];
            if let Err(e) = unsafe { DestroyPhysicalMonitors(&raw) } {
                log::debug!("DestroyPhysicalMonitors failed: {e}");
            }
        }
    }

    pub(super) fn supports_ddc_vcp() -> bool {
        true
    }
}

#[cfg(not(windows))]
mod platform {
    use super::ActionError;

    pub(super) fn set_vcp(
        _monitor_selector: Option<String>,
        _code: u8,
        _value: u32,
    ) -> Result<(), ActionError> {
        Err(ActionError::Unsupported)
    }

    pub(super) fn supports_ddc_vcp() -> bool {
        false
    }
}
