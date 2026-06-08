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

use crate::error::WindowsCaptureCreationError;

use super::{Capture, CaptureError, CaptureEvent, Position};

mod display_util;
mod event_thread;

fn virtual_screen_rect() -> Option<(i32, i32, u32, u32)> {
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((x, y, w as u32, h as u32))
}

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

    /// Virtual-screen union of all monitors. The origin may be
    /// negative when a monitor sits left of / above the primary.
    fn display_rect(&self) -> Option<(i32, i32, u32, u32)> {
        virtual_screen_rect()
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Result<Self, WindowsCaptureCreationError> {
        let (event_tx, event_rx) = channel(1024);
        let event_thread = EventThread::new(event_tx)?;
        Ok(Self {
            event_thread,
            event_rx,
        })
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
