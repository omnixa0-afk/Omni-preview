//! Which holder is asked for which blocks of a file (docs/decisions/attachment-content-tree.md,
//! «Источники», stages 3 and 4). Pure bookkeeping — the pull itself is `app_group_attachments`.
//!
//! Each idle holder is asked for a couple of blocks not here yet, so a fast holder simply comes back
//! for more sooner. With one holder they go in order (a video plays while it loads); with several,
//! the rarest first — the blocks the fewest holders said they have — so what only the author has
//! spreads first and the rest is traded between members. A holder's answer opens with its note:
//! what it has (a copy in progress grows, so this is trusted a few seconds), and — from a seeding
//! author — the blocks it just gave another member, "ask X". Those are asked of X only, again every
//! second while X is still getting them; if X does not come through in time, of anyone, insisting.
//! Asked blocks that did not come are remembered as missing at that holder as long as its word on
//! its map holds. Once every missing block is asked of someone, an idle holder doubles one asked of
//! another (endgame). A block that fails its check costs the holder a strike; two, and it is not
//! asked again for this file; a holder whose stream broke or who refused rests a while.

use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Range;

/// Blocks (16 MiB each) asked of one holder at a time.
pub(crate) const BLOCKS_PER_ASK: u64 = 2;
/// Corrupt answers after which a holder is not asked for this file again.
pub(crate) const STRIKES_TO_EXCLUDE: u32 = 2;
/// A holder that failed (stream broke, refused) is left alone this long.
#[cfg(not(test))]
pub(crate) const REST_AFTER_FAILURE_MS: i64 = 15_000;
#[cfg(test)]
pub(crate) const REST_AFTER_FAILURE_MS: i64 = 3_000;
/// Not a byte from anyone for this long — no one asked has the rest, named members are not getting
/// it, every holder rests: the pull gives up, and the next tick starts it again.
#[cfg(not(test))]
pub(crate) const GIVE_UP_STALLED_MS: i64 = 60_000;
#[cfg(test)]
pub(crate) const GIVE_UP_STALLED_MS: i64 = 40_000;
/// A holder's own word on what it has is trusted this long: a copy in progress grows.
#[cfg(not(test))]
pub(crate) const KNOWN_MAP_TRUST_MS: i64 = 5_000;
#[cfg(test)]
pub(crate) const KNOWN_MAP_TRUST_MS: i64 = 500;
/// A block another holder was named for is asked of that holder only, this long.
#[cfg(not(test))]
pub(crate) const HINT_TRUST_MS: i64 = 15_000;
#[cfg(test)]
pub(crate) const HINT_TRUST_MS: i64 = 30_000;
/// A named holder that did not have the block yet is asked for it again after this.
pub(crate) const HINT_RECHECK_MS: i64 = 1_000;

/// How an ask ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskOutcome {
    /// The holder ended its answer: what of the range did not come, it does not have.
    Answered,
    /// A chunk or a block proof failed its check against the file's tree.
    Corrupt,
    /// The stream broke or the holder refused.
    Failed,
}

/// One ask to start: the holder, its blocks, and whether it may not name someone else for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ask<H> {
    pub(crate) holder: H,
    pub(crate) blocks: Range<u64>,
    pub(crate) insist: bool,
}

#[derive(Default)]
struct HolderState {
    /// Block → until when it counts as missing here.
    missing: HashMap<u64, i64>,
    strikes: u32,
    resting_until_ms: i64,
    /// What it last said it has (`None` — all of it), and when.
    known: Option<(Option<Vec<bool>>, i64)>,
}

pub(crate) struct PullPlan<H> {
    holders: HashMap<H, HolderState>,
    asked: Vec<(H, Range<u64>)>,
    /// Block → the holder named for it, since when.
    hints: HashMap<u64, (H, i64)>,
}

impl<H: Copy + Eq + Hash> PullPlan<H> {
    pub(crate) fn new() -> Self {
        Self { holders: HashMap::new(), asked: Vec::new(), hints: HashMap::new() }
    }

    /// Asks still running.
    pub(crate) fn asked(&self) -> usize {
        self.asked.len()
    }

    /// Holders being asked right now — what the bubble counts as sources.
    pub(crate) fn sources(&self) -> usize {
        let mut holders: Vec<&H> = Vec::new();
        for (holder, _) in &self.asked {
            if !holders.contains(&holder) {
                holders.push(holder);
            }
        }
        holders.len()
    }

    /// Two strikes: not asked again for this file.
    pub(crate) fn excluded(&self, holder: &H) -> bool {
        self.holders.get(holder).is_some_and(|state| state.strikes >= STRIKES_TO_EXCLUDE)
    }

    fn missing_at(&self, holder: &H, block: u64, now_ms: i64) -> bool {
        self.holders
            .get(holder)
            .and_then(|state| state.missing.get(&block))
            .is_some_and(|until| now_ms < *until)
    }

    /// What the holder said it has, while that word holds: `Some(true)` has it, `Some(false)`
    /// lacks it, `None` not known.
    fn said(&self, holder: &H, block: u64, now_ms: i64) -> Option<bool> {
        match &self.holders.get(holder)?.known {
            Some((None, _)) => Some(true),
            Some((Some(map), at)) if now_ms - at < KNOWN_MAP_TRUST_MS => Some(map.get(block as usize).copied().unwrap_or(false)),
            _ => None,
        }
    }

    /// The holder a hint names for `block`, while it can be followed: fresh, one of `holders` (a
    /// member we have a link to — a name outside them is no one to ask), and not resting after a
    /// failure (gone, or refusing: no use waiting out the name).
    fn hinted(&self, block: u64, holders: &[H], now_ms: i64) -> Option<H> {
        self.hints
            .get(&block)
            .filter(|(named, since)| {
                now_ms - since < HINT_TRUST_MS
                    && holders.contains(named)
                    && !self.excluded(named)
                    && self.holders.get(named).is_none_or(|state| state.resting_until_ms <= now_ms)
            })
            .map(|(named, _)| *named)
    }

    fn askers_of(&self, block: u64) -> usize {
        self.asked.iter().filter(|(_, range)| range.contains(&block)).count()
    }

    /// May `holder` be asked for `block` now (asked of anyone or not)?
    fn askable(&self, holder: &H, block: u64, have: &[bool], holders: &[H], now_ms: i64) -> bool {
        !have[block as usize]
            && !self.missing_at(holder, block, now_ms)
            && self.said(holder, block, now_ms) != Some(false)
            && self.hinted(block, holders, now_ms).is_none_or(|named| named == *holder)
    }

    /// Holders of `holders` that said they have `block`.
    fn rarity(&self, block: u64, holders: &[H], now_ms: i64) -> usize {
        holders.iter().filter(|holder| self.said(holder, block, now_ms) == Some(true)).count()
    }

    /// Every holder of `holders` said it has none of the blocks still missing, or failed and rests,
    /// and no one is named for any: nothing to wait for — the pull gives up at once (the next tick
    /// starts it again) instead of waiting out the stall.
    pub(crate) fn nobody_has_the_rest(&self, have: &[bool], holders: &[H], now_ms: i64) -> bool {
        let missing: Vec<u64> = (0..have.len() as u64).filter(|block| !have[*block as usize]).collect();
        missing.iter().all(|block| self.hinted(*block, holders, now_ms).is_none())
            && holders.iter().all(|holder| {
                self.excluded(holder)
                    || self.holders.get(holder).is_some_and(|state| state.resting_until_ms > now_ms)
                    || missing.iter().all(|block| self.said(holder, *block, now_ms) == Some(false))
            })
    }

    /// A holder's note: what it has (`None` — all of it) and the blocks it named someone else for.
    pub(crate) fn noted(&mut self, holder: H, have: Option<Vec<bool>>, elsewhere: Vec<(u64, H)>, now_ms: i64) {
        self.holders.entry(holder).or_default().known = Some((have, now_ms));
        for (block, named) in elsewhere {
            if named != holder {
                self.hints.entry(block).or_insert((named, now_ms));
            }
        }
    }

    /// The asks to start now, one per idle holder of `holders` (in their order of preference).
    pub(crate) fn assign(&mut self, have: &[bool], holders: &[H], now_ms: i64) -> Vec<Ask<H>> {
        let blocks = have.len() as u64;
        let mut started = Vec::new();
        for holder in holders {
            let idle = !self.asked.iter().any(|(busy, _)| busy == holder);
            let resting = self.holders.get(holder).is_some_and(|state| state.resting_until_ms > now_ms);
            if !idle || resting || self.excluded(holder) {
                continue;
            }
            let fresh = |plan: &Self, block: u64| plan.askable(holder, block, have, holders, now_ms) && plan.askers_of(block) == 0;
            // Blocks this holder was named for first; then the rarest (several holders) or the first.
            let named_here = (0..blocks).find(|&block| fresh(self, block) && self.hinted(block, holders, now_ms) == Some(*holder));
            let first = named_here.or_else(|| {
                let mut candidates = (0..blocks).filter(|&block| fresh(self, block));
                if holders.len() > 1 {
                    candidates.min_by_key(|&block| (self.rarity(block, holders, now_ms), block))
                } else {
                    candidates.next()
                }
            });
            let range = match first {
                Some(first) => {
                    let mut end = first + 1;
                    while end < blocks && end - first < BLOCKS_PER_ASK && fresh(self, end) {
                        end += 1;
                    }
                    first..end
                }
                // Endgame: a block asked of one other holder only, asked of this one too.
                None => match (0..blocks).find(|&block| {
                    self.askable(holder, block, have, holders, now_ms) && self.askers_of(block) == 1
                }) {
                    Some(block) => block..block + 1,
                    None => continue,
                },
            };
            // A block someone was named for, that can no longer be asked of them: no more names.
            let insist = range.clone().any(|block| self.hints.contains_key(&block) && self.hinted(block, holders, now_ms).is_none());
            self.asked.push((*holder, range.clone()));
            started.push(Ask { holder: *holder, blocks: range, insist });
        }
        started
    }

    /// An ask ended; `have` is the map after what it brought was counted (its note was `noted`).
    pub(crate) fn finished(&mut self, holder: H, range: Range<u64>, have: &[bool], outcome: AskOutcome, now_ms: i64) {
        if let Some(at) = self.asked.iter().position(|(asked, asked_range)| *asked == holder && *asked_range == range) {
            self.asked.remove(at);
        }
        // A block here needs no one named for it.
        self.hints.retain(|block, _| !have.get(*block as usize).copied().unwrap_or(false));
        let named = |block: &u64| self.hints.get(block).map(|(named, _)| *named);
        let named_elsewhere: Vec<u64> = range.clone().filter(|block| named(block).is_some_and(|named| named != holder)).collect();
        // Named for some block now: it is getting the file, asked again shortly.
        let named_now = self.hints.values().any(|(named, since)| *named == holder && now_ms - since < HINT_TRUST_MS);
        let state = self.holders.entry(holder).or_default();
        match outcome {
            AskOutcome::Answered => {
                for block in range {
                    if have.get(block as usize).copied().unwrap_or(true) || named_elsewhere.contains(&block) {
                        // Here now — or not sent because someone else was named for it: not missing there.
                        continue;
                    }
                    let recheck = if named_now { HINT_RECHECK_MS } else { KNOWN_MAP_TRUST_MS };
                    state.missing.insert(block, now_ms + recheck);
                }
            }
            AskOutcome::Corrupt => {
                state.strikes += 1;
                state.resting_until_ms = now_ms + REST_AFTER_FAILURE_MS;
            }
            AskOutcome::Failed => state.resting_until_ms = now_ms + REST_AFTER_FAILURE_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asks(started: Vec<Ask<&'static str>>) -> Vec<(&'static str, Range<u64>)> {
        started.into_iter().map(|ask| (ask.holder, ask.blocks)).collect()
    }

    #[test]
    fn idle_holders_get_the_next_missing_blocks_in_order_a_couple_each() {
        let mut plan = PullPlan::new();
        let have = [true, false, false, false, false, false];
        assert_eq!(asks(plan.assign(&have, &["a", "b"], 0)), vec![("a", 1..3), ("b", 3..5)]);
        // Busy holders get nothing more; a new one gets what is left.
        assert_eq!(asks(plan.assign(&have, &["a", "b", "c"], 0)), vec![("c", 5..6)]);
        assert_eq!(plan.asked(), 3);
        assert_eq!(plan.sources(), 3);
    }

    #[test]
    fn what_a_holder_did_not_send_is_asked_of_another_and_of_it_again_later() {
        let mut plan = PullPlan::new();
        let mut have = vec![false; 4];
        assert_eq!(asks(plan.assign(&have, &["a"], 0)), vec![("a", 0..2)]);
        // It had block 0, not block 1.
        have[0] = true;
        plan.finished("a", 0..2, &have, AskOutcome::Answered, 10);
        assert_eq!(asks(plan.assign(&have, &["a", "b"], 20)), vec![("a", 2..4), ("b", 1..2)]);
        plan.finished("a", 2..4, &have, AskOutcome::Failed, 30);
        plan.finished("b", 1..2, &have, AskOutcome::Failed, 30);
        // After a while block 1 is asked of "a" again: its copy may have grown.
        let later = 30 + KNOWN_MAP_TRUST_MS.max(REST_AFTER_FAILURE_MS);
        assert_eq!(asks(plan.assign(&have, &["a"], later)), vec![("a", 1..3)]);
    }

    #[test]
    fn at_the_end_an_idle_holder_doubles_a_block_asked_of_another() {
        let mut plan = PullPlan::new();
        let have = [true, true, false];
        assert_eq!(asks(plan.assign(&have, &["slow"], 0)), vec![("slow", 2..3)]);
        assert_eq!(asks(plan.assign(&have, &["slow", "fast"], 0)), vec![("fast", 2..3)]);
        // Never a third time.
        assert_eq!(asks(plan.assign(&have, &["slow", "fast", "third"], 0)), vec![]);
    }

    #[test]
    fn a_holder_that_sends_corrupt_blocks_twice_is_not_asked_again() {
        let mut plan = PullPlan::new();
        let have = [false; 2];
        for round in 0..STRIKES_TO_EXCLUDE as i64 {
            let now = round * (REST_AFTER_FAILURE_MS + 1);
            assert_eq!(asks(plan.assign(&have, &["bad"], now)), vec![("bad", 0..2)]);
            plan.finished("bad", 0..2, &have, AskOutcome::Corrupt, now);
        }
        assert!(plan.excluded(&"bad"));
        assert_eq!(asks(plan.assign(&have, &["bad"], i64::MAX / 2)), vec![]);
        // Another holder is asked meanwhile.
        assert_eq!(asks(plan.assign(&have, &["bad", "good"], i64::MAX / 2)), vec![("good", 0..2)]);
    }

    #[test]
    fn a_pull_has_nothing_to_wait_for_once_every_holder_said_it_lacks_the_rest() {
        let mut plan = PullPlan::new();
        let have = [true, false, false];
        assert!(!plan.nobody_has_the_rest(&have, &["a", "b"], 0), "not known yet");
        plan.noted("a", Some(vec![true, false, false]), Vec::new(), 0);
        assert!(!plan.nobody_has_the_rest(&have, &["a", "b"], 1), "b may have it");
        plan.noted("b", Some(vec![false; 3]), Vec::new(), 0);
        assert!(plan.nobody_has_the_rest(&have, &["a", "b"], 1));
        // Someone named for a block: wait for it.
        plan.noted("author", None, vec![(2, "b")], 0);
        assert!(!plan.nobody_has_the_rest(&have, &["a", "b"], 1));
        // A holder that failed rests: nothing to wait for from it either.
        let mut plan = PullPlan::new();
        assert_eq!(asks(plan.assign(&have, &["gone"], 0)), vec![("gone", 1..3)]);
        plan.finished("gone", 1..3, &have, AskOutcome::Failed, 1);
        assert!(plan.nobody_has_the_rest(&have, &["gone"], 2));
    }

    /// A holder whose note says it has none of the file is not walked through it two blocks at a
    /// time: it is asked again once its word goes stale (its copy grows meanwhile).
    #[test]
    fn a_holder_that_said_it_has_nothing_is_not_asked_until_its_word_goes_stale() {
        let mut plan = PullPlan::new();
        let have = [false; 8];
        assert_eq!(asks(plan.assign(&have, &["empty"], 0)), vec![("empty", 0..2)]);
        plan.noted("empty", Some(vec![false; 8]), Vec::new(), 1);
        plan.finished("empty", 0..2, &have, AskOutcome::Answered, 1);
        assert_eq!(asks(plan.assign(&have, &["empty"], 2)), vec![]);
        assert_eq!(asks(plan.assign(&have, &["empty"], 2 + KNOWN_MAP_TRUST_MS)), vec![("empty", 0..2)]);
    }

    /// Stage 4: with several holders, the blocks the fewest of them have come first — what only the
    /// author has spreads before what members already trade.
    #[test]
    fn with_several_holders_the_rarest_blocks_go_first_and_no_one_is_asked_what_it_said_it_lacks() {
        let mut plan = PullPlan::new();
        let have = [false; 4];
        plan.noted("author", None, Vec::new(), 0);
        plan.noted("member", Some(vec![true, true, false, false]), Vec::new(), 0);
        // The member is asked only for what it has; the author for what only the author has.
        assert_eq!(asks(plan.assign(&have, &["member", "author"], 1)), vec![("member", 0..2), ("author", 2..4)]);
    }

    /// Stage 4: a seeding author names a member for blocks it just gave that member. They are asked
    /// of the member only — again shortly while it is still getting them — and of the author,
    /// insisting, once the name has gone stale; a name outside our holders is no one to ask.
    #[test]
    fn a_named_holder_is_asked_for_its_blocks_and_the_author_insists_once_the_name_goes_stale() {
        let mut plan = PullPlan::new();
        let mut have = vec![false; 4];
        // The author gives blocks 2 and 3, and names "x" for 0 and 1.
        let started = plan.assign(&have, &["author", "x"], 0);
        assert_eq!(asks(started), vec![("author", 0..2), ("x", 2..4)]);
        plan.noted("author", None, vec![(0, "x"), (1, "x")], 1);
        plan.finished("author", 0..2, &have, AskOutcome::Answered, 2);
        plan.noted("x", Some(vec![false; 4]), Vec::new(), 3);
        plan.finished("x", 2..4, &have, AskOutcome::Answered, 3);
        // Named, not rested: "x" is asked for 0..2 — not the author — once it said it lacks them
        // no more, and while the name holds the author is not asked for them at all.
        assert_eq!(asks(plan.assign(&have, &["author", "x"], 4)), vec![("author", 2..4)]);
        let started = plan.assign(&have, &["author", "x"], KNOWN_MAP_TRUST_MS + 4);
        assert_eq!(asks(started), vec![("x", 0..2)]);
        // "x" did not have them yet: asked again a second later.
        plan.finished("x", 0..2, &have, AskOutcome::Answered, KNOWN_MAP_TRUST_MS + 5);
        assert_eq!(asks(plan.assign(&have, &["x"], KNOWN_MAP_TRUST_MS + 6)), vec![]);
        assert_eq!(asks(plan.assign(&have, &["x"], KNOWN_MAP_TRUST_MS + 6 + HINT_RECHECK_MS)), vec![("x", 0..2)]);
        plan.finished("x", 0..2, &have, AskOutcome::Failed, KNOWN_MAP_TRUST_MS + 7 + HINT_RECHECK_MS);
        // The name went stale: the author, insisting.
        have[2] = true;
        have[3] = true;
        plan.finished("author", 2..4, &have, AskOutcome::Answered, HINT_TRUST_MS);
        let started = plan.assign(&have, &["author"], HINT_TRUST_MS + 1);
        assert_eq!(started, vec![Ask { holder: "author", blocks: 0..2, insist: true }]);

        // A name that is not one of our holders is not followed: the author is asked, insisting.
        let mut plan = PullPlan::new();
        let have = [false; 2];
        plan.noted("author", None, vec![(0, "stranger")], 0);
        assert_eq!(plan.assign(&have, &["author"], 1), vec![Ask { holder: "author", blocks: 0..2, insist: true }]);
    }
}
