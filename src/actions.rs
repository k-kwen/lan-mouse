use lan_mouse_ipc::ClientAction;
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub(crate) enum ActionError {
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

#[cfg(windows)]
mod platform {
    use super::ActionError;
    use std::ptr::addr_of_mut;
    use windows::Win32::Devices::Display::{
        DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
        GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetVCPFeature,
    };
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::core::{BOOL, Error as WindowsError};

    struct PhysicalMonitor {
        index: usize,
        raw: PHYSICAL_MONITOR,
        description: String,
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
                .map(|m| format!("{}:{}", m.index, m.description))
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
            monitor.description.to_ascii_lowercase().contains(&selector)
                || selector.contains(&format!("monitor{}", monitor.index))
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

    fn monitor_description(monitor: &PHYSICAL_MONITOR) -> String {
        let description = monitor.szPhysicalMonitorDescription;
        let len = description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(description.len());
        String::from_utf16_lossy(&description[..len])
    }

    impl Drop for PhysicalMonitor {
        fn drop(&mut self) {
            let raw = [self.raw];
            if let Err(e) = unsafe { DestroyPhysicalMonitors(&raw) } {
                log::debug!("DestroyPhysicalMonitors failed: {e}");
            }
        }
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
}
