//! Remote screen control: the viewer's input events, signed, on the media datagram plane.
//!
//! The sharer grants control (group: `GroupCallControlV1::screen_control`; 1:1:
//! `CallScreenControlV1`); the grantee sends batches of input events which the sharer's core
//! verifies against the grantee's identity key before the shell injects them. The signature — not
//! the path, not a media key every member can derive — is what makes an event authentic.
//! See docs/decisions/remote-screen-control.md.
//!
//! Batch body (unsigned part): counter(8) ‖ first_seq(4) ‖ flags(1) ‖ [x(2) ‖ y(2)] ‖ n(1) ‖ n × event(8),
//! followed by an Ed25519 signature (64) over [`screen_control_signed_bytes`].
//!
//! Losses are covered without retransmission: every batch repeats the discrete events (buttons,
//! keys, wheel) of the last moment, numbered consecutively from `first_seq`; the receiver applies
//! only numbers it has not applied yet.

/// 1:1 datagram: MAGIC(2) ‖ body ‖ signature. Never valid RTP/RTCP: their first byte is 0x80..0xBF.
pub const SCREEN_CONTROL_MAGIC: [u8; 2] = [0x53, 0x43]; // "SC"
pub const SCREEN_CONTROL_SIGNATURE_LEN: usize = 64;
/// Discrete events per batch — the redundancy window. Bounded so a batch fits one datagram.
pub const SCREEN_CONTROL_MAX_EVENTS: usize = 32;
const EVENT_LEN: usize = 8;
const FLAG_POINTER: u8 = 0x01;
const SIGNING_DOMAIN: &[u8] = b"omni-screen-control-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenMouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl ScreenMouseButton {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::X1 => 3,
            Self::X2 => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Left,
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::X1,
            4 => Self::X2,
            _ => return None,
        })
    }
}

/// A discrete input event. Pointer position is not one: only the latest position matters, and it
/// rides every batch on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenInputEvent {
    Button { button: ScreenMouseButton, down: bool },
    /// Wheel notches in `WHEEL_DELTA` units (120 = one notch); `dy` positive = away from the user.
    Wheel { dx: i16, dy: i16 },
    /// A key by virtual-key code and hardware scan code (both, so either layout model can inject it).
    Key { vk: u16, scan: u16, extended: bool, down: bool },
}

const KIND_BUTTON: u8 = 1;
const KIND_WHEEL: u8 = 2;
const KIND_KEY: u8 = 3;

impl ScreenInputEvent {
    fn encode(&self) -> [u8; EVENT_LEN] {
        let mut out = [0u8; EVENT_LEN];
        match *self {
            Self::Button { button, down } => {
                out[0] = KIND_BUTTON;
                out[1] = button.code();
                out[2] = down as u8;
            }
            Self::Wheel { dx, dy } => {
                out[0] = KIND_WHEEL;
                out[2..4].copy_from_slice(&dx.to_be_bytes());
                out[4..6].copy_from_slice(&dy.to_be_bytes());
            }
            Self::Key { vk, scan, extended, down } => {
                out[0] = KIND_KEY;
                out[1] = extended as u8;
                out[2] = down as u8;
                out[4..6].copy_from_slice(&vk.to_be_bytes());
                out[6..8].copy_from_slice(&scan.to_be_bytes());
            }
        }
        out
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        let u16_at = |at: usize| u16::from_be_bytes([raw[at], raw[at + 1]]);
        let flag = |at: usize| match raw[at] {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        Some(match raw[0] {
            KIND_BUTTON => Self::Button { button: ScreenMouseButton::from_code(raw[1])?, down: flag(2)? },
            KIND_WHEEL => Self::Wheel { dx: u16_at(2) as i16, dy: u16_at(4) as i16 },
            KIND_KEY => Self::Key { vk: u16_at(4), scan: u16_at(6), extended: flag(1)?, down: flag(2)? },
            _ => return None,
        })
    }
}

/// One batch from a viewer: where the pointer is now, and the discrete events of the last moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenControlBatch {
    /// Strictly increasing per viewer and call; a batch not newer than the last accepted is a replay.
    pub counter: u64,
    /// Pointer position normalised to the shared monitor, 0..=65535 on each axis.
    pub pointer: Option<(u16, u16)>,
    /// Number of `events[0]`; the rest follow consecutively.
    pub first_seq: u32,
    pub events: Vec<ScreenInputEvent>,
}

impl ScreenControlBatch {
    /// The unsigned body; `None` if there are more events than one batch carries.
    pub fn encode_body(&self) -> Option<Vec<u8>> {
        if self.events.len() > SCREEN_CONTROL_MAX_EVENTS {
            return None;
        }
        let mut out = Vec::with_capacity(18 + self.events.len() * EVENT_LEN);
        out.extend_from_slice(&self.counter.to_be_bytes());
        out.extend_from_slice(&self.first_seq.to_be_bytes());
        out.push(if self.pointer.is_some() { FLAG_POINTER } else { 0 });
        if let Some((x, y)) = self.pointer {
            out.extend_from_slice(&x.to_be_bytes());
            out.extend_from_slice(&y.to_be_bytes());
        }
        out.push(self.events.len() as u8);
        for event in &self.events {
            out.extend_from_slice(&event.encode());
        }
        Some(out)
    }

    /// Parse an unsigned body; `None` for anything malformed (including trailing bytes).
    pub fn decode_body(body: &[u8]) -> Option<Self> {
        let counter = u64::from_be_bytes(body.get(0..8)?.try_into().ok()?);
        let first_seq = u32::from_be_bytes(body.get(8..12)?.try_into().ok()?);
        let flags = *body.get(12)?;
        if flags & !FLAG_POINTER != 0 {
            return None;
        }
        let mut at = 13;
        let pointer = if flags & FLAG_POINTER != 0 {
            let xy = body.get(at..at + 4)?;
            at += 4;
            Some((u16::from_be_bytes([xy[0], xy[1]]), u16::from_be_bytes([xy[2], xy[3]])))
        } else {
            None
        };
        let count = *body.get(at)? as usize;
        at += 1;
        if count > SCREEN_CONTROL_MAX_EVENTS || body.len() != at + count * EVENT_LEN {
            return None;
        }
        let events = body[at..]
            .chunks_exact(EVENT_LEN)
            .map(ScreenInputEvent::decode)
            .collect::<Option<Vec<_>>>()?;
        Some(Self { counter, pointer, first_seq, events })
    }
}

/// What the viewer signs and the sharer verifies: domain ‖ call id ‖ the sharer's identity key ‖ the
/// unsigned body. Binding the call and the sharer keeps a batch from being replayed into another
/// call or at another sharer; the counter inside the body keeps it from being replayed here.
pub fn screen_control_signed_bytes(call_id: &str, sharer_pub: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNING_DOMAIN.len() + 2 + call_id.len() + 32 + body.len());
    out.extend_from_slice(SIGNING_DOMAIN);
    out.extend_from_slice(&(call_id.len() as u16).to_be_bytes());
    out.extend_from_slice(call_id.as_bytes());
    out.extend_from_slice(sharer_pub);
    out.extend_from_slice(body);
    out
}

/// Split `body ‖ signature`; `None` if it is too short to hold a signature.
pub fn split_screen_control_signature(signed: &[u8]) -> Option<(&[u8], [u8; SCREEN_CONTROL_SIGNATURE_LEN])> {
    let at = signed.len().checked_sub(SCREEN_CONTROL_SIGNATURE_LEN)?;
    Some((&signed[..at], signed[at..].try_into().ok()?))
}

/// 1:1 datagram from a signed batch (`body ‖ signature`).
pub fn wrap_screen_control(signed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + signed.len());
    out.extend_from_slice(&SCREEN_CONTROL_MAGIC);
    out.extend_from_slice(signed);
    out
}

/// The signed batch of a 1:1 screen-control datagram; `None` for anything else (e.g. RTP).
pub fn parse_screen_control(datagram: &[u8]) -> Option<&[u8]> {
    datagram.strip_prefix(&SCREEN_CONTROL_MAGIC)
}

/// What to do with an accepted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenControlStep {
    pub pointer: Option<(u16, u16)>,
    /// Events not applied before, in order.
    pub events: Vec<ScreenInputEvent>,
    /// Events were lost beyond the redundancy window: release everything held before applying.
    pub gap: bool,
}

/// The sharer's per-viewer receive state: replay protection and redundancy de-duplication. It sees
/// only batches whose signature already verified.
#[derive(Debug, Default, Clone)]
pub struct ScreenControlReceiver {
    last_counter: Option<u64>,
    /// Number of the next discrete event to apply.
    next_seq: Option<u32>,
}

impl ScreenControlReceiver {
    /// `None` for a batch that is not newer than the last accepted one (replay or reordering — a
    /// late batch only repeats what a newer one already carried).
    pub fn accept(&mut self, batch: &ScreenControlBatch) -> Option<ScreenControlStep> {
        if self.last_counter.is_some_and(|last| batch.counter <= last) {
            return None;
        }
        self.last_counter = Some(batch.counter);
        let end = batch.first_seq.wrapping_add(batch.events.len() as u32);
        let next = self.next_seq.unwrap_or(batch.first_seq);
        // Signed distance, so the u32 counter may wrap.
        let skip = next.wrapping_sub(batch.first_seq) as i32;
        let gap = skip < 0;
        let skip = skip.max(0) as usize;
        let events = batch.events.iter().skip(skip).copied().collect();
        if (end.wrapping_sub(next) as i32) > 0 || self.next_seq.is_none() {
            self.next_seq = Some(end);
        }
        Some(ScreenControlStep { pointer: batch.pointer, events, gap })
    }
}

/// How long a discrete event keeps riding later batches: the redundancy window. The viewer sends a
/// batch at least every [`SCREEN_CONTROL_KEEPALIVE_MS`], so an event lost in one batch reaches the
/// sharer in one of the next two.
pub const SCREEN_CONTROL_REDUNDANCY_MS: i64 = 250;
/// The viewer's keep-alive cadence while controlling, even with nothing new to say.
pub const SCREEN_CONTROL_KEEPALIVE_MS: i64 = 100;

/// The viewer's per-sharer send state: the batch counter and the redundancy window.
#[derive(Debug, Default, Clone)]
pub struct ScreenControlOutbox {
    counter: u64,
    next_seq: u32,
    /// (number, sent at, event), oldest first.
    window: std::collections::VecDeque<(u32, i64, ScreenInputEvent)>,
}

impl ScreenControlOutbox {
    /// The next batch: `events` are numbered and join the window, events older than
    /// [`SCREEN_CONTROL_REDUNDANCY_MS`] (or beyond [`SCREEN_CONTROL_MAX_EVENTS`]) leave it.
    pub fn batch(&mut self, now_ms: i64, pointer: Option<(u16, u16)>, events: &[ScreenInputEvent]) -> ScreenControlBatch {
        for event in events {
            self.window.push_back((self.next_seq, now_ms, *event));
            self.next_seq = self.next_seq.wrapping_add(1);
        }
        while self.window.len() > SCREEN_CONTROL_MAX_EVENTS
            || self.window.front().is_some_and(|(_, at, _)| now_ms - at > SCREEN_CONTROL_REDUNDANCY_MS)
        {
            self.window.pop_front();
        }
        self.counter += 1;
        ScreenControlBatch {
            counter: self.counter,
            pointer,
            first_seq: self.window.front().map_or(self.next_seq, |(seq, _, _)| *seq),
            events: self.window.iter().map(|(_, _, event)| *event).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vk: u16, down: bool) -> ScreenInputEvent {
        ScreenInputEvent::Key { vk, scan: vk + 0x100, extended: vk.is_multiple_of(2), down }
    }

    fn batch(counter: u64, first_seq: u32, events: Vec<ScreenInputEvent>) -> ScreenControlBatch {
        ScreenControlBatch { counter, pointer: Some((counter as u16, 7)), first_seq, events }
    }

    #[test]
    fn a_batch_round_trips_and_garbage_does_not_parse() {
        let all = ScreenControlBatch {
            counter: u64::MAX - 3,
            pointer: Some((0, 65535)),
            first_seq: u32::MAX,
            events: vec![
                ScreenInputEvent::Button { button: ScreenMouseButton::X2, down: true },
                ScreenInputEvent::Wheel { dx: -120, dy: 240 },
                key(0x41, false),
                key(0x2E, true),
            ],
        };
        let body = all.encode_body().unwrap();
        assert_eq!(ScreenControlBatch::decode_body(&body), Some(all.clone()));
        let no_pointer = ScreenControlBatch { pointer: None, events: vec![], ..all.clone() };
        assert_eq!(ScreenControlBatch::decode_body(&no_pointer.encode_body().unwrap()), Some(no_pointer));

        // Negative: truncated, trailing byte, unknown kind / button / flag, too many events.
        assert_eq!(ScreenControlBatch::decode_body(&body[..body.len() - 1]), None);
        assert_eq!(ScreenControlBatch::decode_body(&[body.clone(), vec![0]].concat()), None);
        let mut unknown_kind = body.clone();
        let first_event = body.len() - 4 * EVENT_LEN;
        unknown_kind[first_event] = 9;
        assert_eq!(ScreenControlBatch::decode_body(&unknown_kind), None);
        let mut unknown_button = body.clone();
        unknown_button[first_event + 1] = 5;
        assert_eq!(ScreenControlBatch::decode_body(&unknown_button), None);
        let mut half_down = body.clone();
        half_down[first_event + 2] = 2;
        assert_eq!(ScreenControlBatch::decode_body(&half_down), None);
        let mut unknown_flag = body;
        unknown_flag[12] |= 0x80;
        assert_eq!(ScreenControlBatch::decode_body(&unknown_flag), None);
        let too_many = batch(1, 0, vec![key(1, true); SCREEN_CONTROL_MAX_EVENTS + 1]);
        assert_eq!(too_many.encode_body(), None);
    }

    #[test]
    fn the_signed_bytes_bind_the_call_and_the_sharer() {
        let body = batch(1, 0, vec![key(0x41, true)]).encode_body().unwrap();
        let signed = screen_control_signed_bytes("call-1", &[1; 32], &body);
        assert_ne!(signed, screen_control_signed_bytes("call-2", &[1; 32], &body), "another call");
        assert_ne!(signed, screen_control_signed_bytes("call-1", &[2; 32], &body), "another sharer");
        // The length prefix keeps "call-1" ‖ sharer from colliding with a longer call id.
        assert_ne!(
            screen_control_signed_bytes("ab", &[0x63; 32], &body),
            screen_control_signed_bytes("abc", &[0x63; 32], &body)
        );
    }

    #[test]
    fn a_1to1_datagram_is_told_apart_from_rtp() {
        let signed = [batch(1, 0, vec![]).encode_body().unwrap(), vec![0xAB; 64]].concat();
        let datagram = wrap_screen_control(&signed);
        assert_eq!(parse_screen_control(&datagram), Some(signed.as_slice()));
        let (body, signature) = split_screen_control_signature(&signed).unwrap();
        assert_eq!(signature, [0xAB; 64]);
        assert!(ScreenControlBatch::decode_body(body).is_some());
        // Negative: RTP (version 2) and RTCP never carry the magic; too short for a signature.
        assert_eq!(parse_screen_control(&[0x80, 0x60, 0, 1]), None);
        assert_eq!(parse_screen_control(&[0x81, 0xC9, 0, 1]), None);
        assert_eq!(split_screen_control_signature(&[0; 63]), None);
    }

    #[test]
    fn redundant_events_apply_once_and_replays_not_at_all() {
        let mut rx = ScreenControlReceiver::default();
        let a = key(0x41, true);
        let b = key(0x41, false);
        let c = ScreenInputEvent::Button { button: ScreenMouseButton::Left, down: true };
        let first = rx.accept(&batch(10, 5, vec![a])).unwrap();
        assert_eq!((first.events, first.gap), (vec![a], false));
        // The next batch repeats `a` (redundancy) and adds `b`, `c`: only the new ones apply.
        let second = rx.accept(&batch(11, 5, vec![a, b, c])).unwrap();
        assert_eq!((second.events, second.gap), (vec![b, c], false));
        // Keep-alive with the same window: pointer moves, nothing re-applies.
        let idle = rx.accept(&batch(12, 6, vec![b, c])).unwrap();
        assert_eq!((idle.events, idle.pointer), (vec![], Some((12, 7))));

        // Negative: a replayed or late batch is refused whole, even if it carries events.
        assert_eq!(rx.accept(&batch(12, 6, vec![b, c])), None);
        assert_eq!(rx.accept(&batch(3, 99, vec![a])), None);
    }

    #[test]
    fn the_outbox_repeats_recent_events_and_lets_old_ones_go() {
        let mut out = ScreenControlOutbox::default();
        let (a, b) = (key(1, true), key(1, false));
        let first = out.batch(1_000, Some((1, 2)), &[a]);
        assert_eq!((first.counter, first.first_seq, first.events.clone()), (1, 0, vec![a]));
        let second = out.batch(1_100, None, &[b]);
        assert_eq!((second.counter, second.first_seq, second.events.clone()), (2, 0, vec![a, b]));
        // A keep-alive past the window: `a` has aged out, `b` still rides.
        let third = out.batch(1_000 + SCREEN_CONTROL_REDUNDANCY_MS + 1, None, &[]);
        assert_eq!((third.first_seq, third.events.clone()), (1, vec![b]));
        // Fed to a receiver that lost the second batch, nothing is missed and nothing repeats.
        let mut rx = ScreenControlReceiver::default();
        assert_eq!(rx.accept(&first).unwrap().events, vec![a]);
        assert_eq!(rx.accept(&third).unwrap().events, vec![b]);

        // Negative: an idle outbox sends an empty window numbered where the next event will be, and a
        // burst never exceeds one batch.
        let idle = out.batch(10_000, None, &[]);
        assert_eq!((idle.first_seq, idle.events.len()), (2, 0));
        let burst = out.batch(10_001, None, &[key(3, true); SCREEN_CONTROL_MAX_EVENTS + 5]);
        assert_eq!(burst.events.len(), SCREEN_CONTROL_MAX_EVENTS);
        assert!(burst.encode_body().is_some());
    }

    #[test]
    fn events_lost_beyond_the_window_flag_a_gap() {
        let mut rx = ScreenControlReceiver::default();
        rx.accept(&batch(1, 0, vec![key(1, true)])).unwrap();
        // Events 1..=3 never arrived in any batch; 4 is the first we see.
        let step = rx.accept(&batch(5, 4, vec![key(1, false)])).unwrap();
        assert!(step.gap, "keys may be held down that were released in the lost events");
        assert_eq!(step.events, vec![key(1, false)]);
        // Negative: contiguous numbering is not a gap, including across u32 wrap.
        let mut wrap = ScreenControlReceiver::default();
        wrap.accept(&batch(1, u32::MAX, vec![key(2, true)])).unwrap();
        let step = wrap.accept(&batch(2, 0, vec![key(2, false)])).unwrap();
        assert_eq!((step.gap, step.events), (false, vec![key(2, false)]));
    }
}
