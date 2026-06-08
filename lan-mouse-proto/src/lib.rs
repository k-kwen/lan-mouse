use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// event type is a retired wire variant
    #[error("unsupported event: `{0}`")]
    UnsupportedEvent(&'static str),
    /// event payload length does not match the wire format
    #[error("truncated event `{event}`: expected {expected} bytes, got {actual}")]
    Truncated {
        event: &'static str,
        expected: usize,
        actual: usize,
    },
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Display geometry of the receiving device. Sent by the
    /// emulation side immediately after the [`ProtoEvent::Ack`] of
    /// an [`ProtoEvent::Enter`] so the capturing peer can model the
    /// guest cursor's position along the entry axis. Width and
    /// height are in pixels of the union of all displays on the
    /// emulating device.
    Bounds { width: u32, height: u32 },
    /// Cursor warp on the receiving device.
    /// Carries the host's cursor position normalized to the host's
    /// own display bounds (0..1 along each axis) plus the entry
    /// side from the receiver's frame. The receiver scales nx/ny
    /// against its own bounds and pins the on-axis dimension to the
    /// entry edge.
    CursorPos { pos: Position, nx: f32, ny: f32 },
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Bounds { width, height } => write!(f, "Bounds({width}x{height})"),
            ProtoEvent::CursorPos { pos, nx, ny } => {
                write!(f, "CursorPos({pos}, {nx:.4}, {ny:.4})")
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion = 0,
    PointerButton = 1,
    PointerAxis = 2,
    PointerAxisValue120 = 3,
    KeyboardKey = 4,
    KeyboardModifiers = 5,
    Ping = 6,
    Pong = 7,
    Enter = 8,
    Leave = 9,
    Ack = 10,
    Bounds = 11,
    CursorPos = 13,
    Hello = 14,
}

const RETIRED_MOTION_ABSOLUTE_EVENT_ID: u8 = 12;

fn event_wire_len(event_type: u8) -> Result<(&'static str, usize), ProtocolError> {
    if event_type == RETIRED_MOTION_ABSOLUTE_EVENT_ID {
        return Err(ProtocolError::UnsupportedEvent("MotionAbsolute"));
    }

    let event_type = EventType::try_from(event_type)?;
    let spec = match event_type {
        EventType::PointerMotion => ("PointerMotion", 1 + 4 + 8 + 8),
        EventType::PointerButton => ("PointerButton", 1 + 4 + 4 + 4),
        EventType::PointerAxis => ("PointerAxis", 1 + 4 + 1 + 8),
        EventType::PointerAxisValue120 => ("PointerAxisValue120", 1 + 1 + 4),
        EventType::KeyboardKey => ("KeyboardKey", 1 + 4 + 4 + 1),
        EventType::KeyboardModifiers => ("KeyboardModifiers", 1 + 4 + 4 + 4 + 4),
        EventType::Ping => ("Ping", 1),
        EventType::Pong => ("Pong", 1 + 1),
        EventType::Enter => ("Enter", 1 + 1),
        EventType::Leave => ("Leave", 1 + 4),
        EventType::Ack => ("Ack", 1 + 4),
        EventType::Bounds => ("Bounds", 1 + 4 + 4),
        EventType::CursorPos => ("CursorPos", 1 + 1 + 4 + 4),
        EventType::Hello => ("Hello", 1 + 8),
    };
    Ok(spec)
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Bounds { .. } => EventType::Bounds,
            ProtoEvent::CursorPos { .. } => EventType::CursorPos,
            ProtoEvent::Hello { .. } => EventType::Hello,
        }
    }
}

impl TryFrom<&[u8]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: &[u8]) -> Result<Self, Self::Error> {
        let Some((&event_type, _)) = buf.split_first() else {
            return Err(ProtocolError::Truncated {
                event: "<event-type>",
                expected: 1,
                actual: 0,
            });
        };
        let (name, expected) = event_wire_len(event_type)?;
        if buf.len() != expected {
            return Err(ProtocolError::Truncated {
                event: name,
                expected,
                actual: buf.len(),
            });
        }

        let mut buf = buf;
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Bounds => Ok(Self::Bounds {
                width: decode_u32(&mut buf)?,
                height: decode_u32(&mut buf)?,
            }),
            EventType::CursorPos => Ok(Self::CursorPos {
                pos: decode_u8(&mut buf)?.try_into()?,
                nx: decode_f32(&mut buf)?,
                ny: decode_f32(&mut buf)?,
            }),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        Self::try_from(&buf[..])
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Bounds { width, height } => {
                    encode_u32(buf, len, width);
                    encode_u32(buf, len, height);
                }
                ProtoEvent::CursorPos { pos, nx, ny } => {
                    encode_u8(buf, len, pos as u8);
                    encode_f32(buf, len, nx);
                    encode_f32(buf, len, ny);
                }
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_motion_absolute_event_id_is_unsupported() {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        buf[0] = RETIRED_MOTION_ABSOLUTE_EVENT_ID;

        assert!(matches!(
            ProtoEvent::try_from(buf),
            Err(ProtocolError::UnsupportedEvent("MotionAbsolute"))
        ));
    }

    #[test]
    fn truncated_pong_is_rejected() {
        let buf = [EventType::Pong as u8];

        assert!(matches!(
            ProtoEvent::try_from(&buf[..]),
            Err(ProtocolError::Truncated {
                event: "Pong",
                expected: 2,
                actual: 1,
            })
        ));
    }

    #[test]
    fn short_ping_is_accepted_but_padded_ping_is_rejected() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Ping.into();

        assert!(matches!(
            ProtoEvent::try_from(&buf[..len]),
            Ok(ProtoEvent::Ping)
        ));
        assert!(matches!(
            ProtoEvent::try_from(&buf[..]),
            Err(ProtocolError::Truncated {
                event: "Ping",
                expected: 1,
                actual: MAX_EVENT_SIZE,
            })
        ));
    }

    #[test]
    fn cursor_pos_requires_exact_payload_length() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::CursorPos {
            pos: Position::Left,
            nx: 0.5,
            ny: 0.25,
        }
        .into();

        assert!(matches!(
            ProtoEvent::try_from(&buf[..len - 1]),
            Err(ProtocolError::Truncated {
                event: "CursorPos",
                expected: 10,
                actual,
            }) if actual == len - 1
        ));
        assert!(matches!(
            ProtoEvent::try_from(&buf[..len]),
            Ok(ProtoEvent::CursorPos {
                pos: Position::Left,
                ..
            })
        ));
    }
}
