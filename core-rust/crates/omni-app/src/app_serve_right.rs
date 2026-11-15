//! The right to keep sending to a peer, asked again while a stream runs
//! (docs/decisions/serve-right-recheck.md). A stream checked its right when it opened and never
//! again: a member removed from the group, a contact blocked, a profile signed out kept receiving
//! for as long as the stream lasted (audit 2026-11-09, class A).

use std::time::{Duration, Instant};

use omni_types::OmniResult;

/// After this much went out the right is asked again.
pub(crate) const SERVE_RECHECK_BYTES: u64 = 1024 * 1024;
/// …or after this long, whichever comes first: a slow stream is not given a slow answer.
pub(crate) const SERVE_RECHECK_EVERY: Duration = Duration::from_secs(1);

/// A stream's right to go on: asked before the first bytes, then again once [`SERVE_RECHECK_BYTES`]
/// went out or [`SERVE_RECHECK_EVERY`] passed since it last was. After the right is gone the peer gets
/// at most that much more. Asked before the first bytes too: a peer that lost its right and asks
/// again gets nothing, not a mebibyte per request.
pub(crate) struct ServeRight<'a> {
    check: Box<dyn FnMut() -> OmniResult<()> + 'a>,
    bytes: u64,
    since: Instant,
}

impl<'a> ServeRight<'a> {
    /// `check`: an error when the right is gone (its text says why).
    pub(crate) fn new(check: impl FnMut() -> OmniResult<()> + 'a) -> Self {
        Self { check: Box::new(check), bytes: SERVE_RECHECK_BYTES, since: Instant::now() }
    }

    /// Before the next `bytes` go out: asks the right again when it is due. An error: stop sending.
    pub(crate) fn before_sending(&mut self, bytes: u64) -> OmniResult<()> {
        if self.bytes >= SERVE_RECHECK_BYTES || self.since.elapsed() >= SERVE_RECHECK_EVERY {
            (self.check)()?;
            self.bytes = 0;
            self.since = Instant::now();
        }
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_types::OmniError;
    use std::cell::Cell;

    #[test]
    fn the_right_is_asked_again_after_a_mebibyte_and_a_lost_right_stops_the_next_send() {
        let (asked, allowed) = (Cell::new(0), Cell::new(true));
        let mut right = ServeRight::new(|| {
            asked.set(asked.get() + 1);
            if allowed.get() { Ok(()) } else { Err(OmniError::InvalidInput("gone".into())) }
        });
        let chunk = 64 * 1024;
        right.before_sending(chunk).unwrap();
        assert_eq!(asked.get(), 1, "asked before the first bytes");
        for _ in 1..(SERVE_RECHECK_BYTES / chunk) {
            right.before_sending(chunk).unwrap();
        }
        assert_eq!(asked.get(), 1, "not again before a mebibyte went out");
        right.before_sending(chunk).unwrap();
        assert_eq!(asked.get(), 2);
        allowed.set(false);
        for _ in 0..(SERVE_RECHECK_BYTES / chunk) - 1 {
            right.before_sending(chunk).unwrap();
        }
        assert!(right.before_sending(chunk).is_err(), "the next mebibyte does not go");
    }

    #[test]
    fn a_slow_stream_is_asked_again_after_a_second() {
        let asked = Cell::new(0);
        let mut right = ServeRight::new(|| {
            asked.set(asked.get() + 1);
            Ok(())
        });
        right.before_sending(1).unwrap();
        right.before_sending(1).unwrap();
        assert_eq!(asked.get(), 1, "a byte after the first check is not asked about");
        right.since -= SERVE_RECHECK_EVERY;
        right.before_sending(1).unwrap();
        assert_eq!(asked.get(), 2);
    }
}
