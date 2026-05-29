use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::{Capture, CaptureError, CaptureEvent, Position};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.create(pos);
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.destroy(pos);
        Ok(())
    }

    async fn release(&mut self, _warp_target: Option<(i32, i32)>) -> Result<(), CaptureError> {
        self.event_thread.release_capture();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    /// Virtual-screen dimensions (union of all monitors), in pixels.
    /// Mirrors the macOS backend so the proportional `CursorPos` warp
    /// is sent on multi-monitor Windows hosts too — without this the
    /// default `None` disables that warp and the peer falls back to the
    /// entry-edge midpoint, landing the cursor at the wrong spot when
    /// crossing in from a non-primary monitor.
    fn display_bounds(&self) -> Option<(u32, u32)> {
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if w <= 0 || h <= 0 {
            return None;
        }
        Some((w as u32, h as u32))
    }

    /// Top-left corner of the virtual screen. NEGATIVE when a monitor
    /// sits left of / above the primary (Windows anchors the global
    /// coordinate system at the primary's top-left). The default (0, 0)
    /// is wrong for those layouts: `host_normalized_cursor`'s
    /// `clamp(0, 1)` then maps every point on a negative-origin monitor
    /// to the screen edge, so the peer warps to the wrong column/row.
    fn display_origin(&self) -> (i32, i32) {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            )
        }
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        let (event_tx, event_rx) = channel(1024);
        let event_thread = EventThread::new(event_tx);
        Self {
            event_thread,
            event_rx,
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
