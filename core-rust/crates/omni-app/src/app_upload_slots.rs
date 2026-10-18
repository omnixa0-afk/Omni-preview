//! Upload slots and shared block reads (docs/decisions/attachment-content-tree.md, «Отправитель,
//! которого качают многие», C3).
//!
//! One file of ours streamed to many at once — a group, the same file in several chats, a contact's
//! several devices — goes to at most `UPLOAD_SLOTS_PER_FILE` of them at a time; the others wait in
//! line, and a holder that has had its slice steps to the back of the line at a block boundary when
//! someone waits. Everyone moves, none starves, and the disk is not asked for a dozen far-apart
//! places at once. A waiting stream just pauses: the receiver's re-asks for the same transfer are
//! dropped as duplicates while its sender lives.
//!
//! A block read and checked against the tree for one receiver is kept for the others streaming the
//! same file (a few blocks, while the file has holders): receivers that start together read it from
//! disk once.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use omni_types::OmniResult;

/// Streams of one file at a time.
pub(crate) const UPLOAD_SLOTS_PER_FILE: usize = 4;
/// How long a holder streams before it lets the next in line have its slot.
pub(crate) const UPLOAD_SLICE: Duration = Duration::from_secs(30);
/// Checked blocks kept per file while it has holders (16 MiB each).
const SHARED_BLOCKS_PER_FILE: usize = 4;

#[derive(Default)]
struct Line {
    holders: usize,
    waiting: VecDeque<u64>,
    /// (block index, the block) — the newest last.
    blocks: VecDeque<(u64, Arc<SharedBlock>)>,
}

/// A block read from the source and checked against the tree: its bytes and its leaf hashes.
pub(crate) struct SharedBlock {
    pub data: Vec<u8>,
    pub leaves: Vec<omni_crypto::ContentHash>,
}

struct Slots {
    lines: Mutex<HashMap<String, Line>>,
    turn: Condvar,
}

fn slots() -> &'static Slots {
    static SLOTS: OnceLock<Slots> = OnceLock::new();
    SLOTS.get_or_init(|| Slots { lines: Mutex::new(HashMap::new()), turn: Condvar::new() })
}

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// A stream's place among the holders of a file (keyed by its content id). Dropped, it frees the
/// slot; the file's kept blocks go with its last holder.
pub(crate) struct UploadSlot {
    file: String,
    since: Instant,
}

/// Waits for a slot of `file` (first come, first served).
pub(crate) fn acquire_upload_slot(file: &str) -> UploadSlot {
    let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let slots = slots();
    let mut lines = slots.lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lines.entry(file.to_string()).or_default().waiting.push_back(ticket);
    loop {
        let line = lines.get_mut(file).expect("a waiting ticket keeps its line");
        if line.holders < UPLOAD_SLOTS_PER_FILE && line.waiting.front() == Some(&ticket) {
            line.waiting.pop_front();
            line.holders += 1;
            break;
        }
        lines = slots.turn.wait(lines).unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    // The next in line may fit as well.
    slots.turn.notify_all();
    UploadSlot { file: file.to_string(), since: Instant::now() }
}

impl UploadSlot {
    /// At a block boundary: after `slice`, if someone waits for this file, step to the back of the
    /// line (the stream pauses until its turn comes round again).
    pub(crate) fn take_turns(&mut self, slice: Duration) {
        if self.since.elapsed() < slice || !someone_waits(&self.file) {
            return;
        }
        let file = self.file.clone();
        release(&file, false);
        let next = acquire_upload_slot(&file);
        self.since = next.since;
        std::mem::forget(next); // this slot carries on as the one it replaced
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        release(&self.file, true);
    }
}

fn someone_waits(file: &str) -> bool {
    let lines = slots().lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lines.get(file).is_some_and(|line| !line.waiting.is_empty())
}

fn release(file: &str, last_use: bool) {
    let slots = slots();
    let mut lines = slots.lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(line) = lines.get_mut(file) {
        line.holders = line.holders.saturating_sub(1);
        if last_use && line.holders == 0 && line.waiting.is_empty() {
            lines.remove(file);
        }
    }
    slots.turn.notify_all();
}

/// Block `block` of `file`: kept from another holder's read, or read (and checked) by `read` now
/// and kept for the others.
pub(crate) fn shared_block(
    file: &str,
    block: u64,
    read: impl FnOnce() -> OmniResult<SharedBlock>,
) -> OmniResult<Arc<SharedBlock>> {
    {
        let lines = slots().lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, kept)) = lines.get(file).and_then(|line| line.blocks.iter().find(|(at, _)| *at == block)) {
            return Ok(Arc::clone(kept));
        }
    }
    // Read outside the lock: the disk is slow and the other files' streams must not wait on it.
    let read = Arc::new(read()?);
    let mut lines = slots().lines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(line) = lines.get_mut(file) {
        if !line.blocks.iter().any(|(at, _)| *at == block) {
            line.blocks.push_back((block, Arc::clone(&read)));
            while line.blocks.len() > SHARED_BLOCKS_PER_FILE {
                line.blocks.pop_front();
            }
        }
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn file(name: &str) -> String {
        format!("{name}-{}", NEXT_TICKET.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn a_file_streams_to_four_at_a_time_and_the_fifth_waits_its_turn() {
        let file = file("slots");
        let held: Vec<UploadSlot> = (0..UPLOAD_SLOTS_PER_FILE).map(|_| acquire_upload_slot(&file)).collect();
        let fifth = {
            let file = file.clone();
            std::thread::spawn(move || {
                let _slot = acquire_upload_slot(&file);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!fifth.is_finished(), "the fifth waits while four stream");
        assert!(someone_waits(&file));
        drop(held);
        fifth.join().unwrap();
        // Another file is not held up by this one's line.
        let _other = acquire_upload_slot(&super::tests::file("other"));
    }

    #[test]
    fn a_holder_steps_back_after_its_slice_only_when_someone_waits() {
        let file = file("turns");
        let mut holders: Vec<UploadSlot> = (0..UPLOAD_SLOTS_PER_FILE).map(|_| acquire_upload_slot(&file)).collect();
        // Nobody waits: the slice changes nothing.
        holders[0].take_turns(Duration::ZERO);
        let served = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let (file, served) = (file.clone(), Arc::clone(&served));
            std::thread::spawn(move || {
                let _slot = acquire_upload_slot(&file);
                served.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            })
        };
        while !someone_waits(&file) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Within its slice a holder keeps streaming.
        holders[0].take_turns(Duration::from_secs(3600));
        assert_eq!(served.load(Ordering::SeqCst), 0);
        // Past it, it lets the waiter in — and gets its slot back once the waiter is done.
        holders[0].take_turns(Duration::ZERO);
        assert_eq!(served.load(Ordering::SeqCst), 1);
        waiter.join().unwrap();
        drop(holders);
        let lines = slots().lines.lock().unwrap();
        assert!(!lines.contains_key(&file), "the last holder takes the line with it");
    }

    #[test]
    fn a_block_read_for_one_holder_is_kept_for_the_others_of_the_same_file() {
        let file = file("blocks");
        let _a = acquire_upload_slot(&file);
        let _b = acquire_upload_slot(&file);
        let reads = AtomicUsize::new(0);
        let read = || {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(SharedBlock { data: vec![7; 4], leaves: Vec::new() })
        };
        let first = shared_block(&file, 3, read).unwrap();
        let again = shared_block(&file, 3, || unreachable!("kept")).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        for block in 10..10 + SHARED_BLOCKS_PER_FILE as u64 {
            shared_block(&file, block, || Ok(SharedBlock { data: Vec::new(), leaves: Vec::new() })).unwrap();
        }
        let mut reread = false;
        shared_block(&file, 3, || {
            reread = true;
            Ok(SharedBlock { data: Vec::new(), leaves: Vec::new() })
        })
        .unwrap();
        assert!(reread, "only the last few blocks are kept");
    }
}
