//! Outgoing files prepared in the background (docs/decisions/attachment-content-tree.md, «Подготовка
//! у отправителя», C2). Reading the content tree of a terabyte takes hours on a disk; the message is
//! in the chat from the first second as «Подготовка N%», and its offer leaves once the tree is read:
//! a 1:1 message stays queued (the queue does not take a file being prepared), a group offer is sent
//! from here. A preparation cut short by a restart picks up at the blocks it had read
//! (`resume_attachment_preparations`, from the queue tick; the checkpoint is the storage's).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use omni_storage::Storage;
use omni_types::{AttachmentTransferState, OmniResult};

use crate::{current_unix_ms, omni_app_trace_log};

/// A file up to this size is prepared on the spot, as before: the tree of 32 MiB is read in a blink,
/// and the offer goes out with the message. Tests put their 1 MB files through the background path.
#[cfg(not(test))]
pub(crate) const PREPARE_INLINE_MAX_BYTES: i64 = 32 * 1024 * 1024;
#[cfg(test)]
pub(crate) const PREPARE_INLINE_MAX_BYTES: i64 = 512 * 1024;
/// How often the bubble hears how far the reading got (and the pass, whether it was cancelled).
const PROGRESS_EVERY_BYTES: u64 = 64 * 1024 * 1024;

/// (db path, source file) of the preparations running in this process — one pass per file, for
/// every message that sends it (C3) — and the flag that stops it. A pass removes its own entry when it ends; the profile stopping only
/// raises the flags (`stop_attachment_preparations_for_profile`).
type Running = Mutex<HashMap<(String, String), Arc<AtomicBool>>>;

fn running() -> &'static Running {
    static RUNNING: OnceLock<Running> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The profile stops (`stop_runtime_context`): its passes let go of the database within a leaf —
/// a pass reading a terabyte must not keep the profile's files open for hours (a deleted profile
/// could not be removed). The rows stay `preparing`; the next start picks up where the reading was.
pub(crate) fn stop_attachment_preparations_for_profile(db_path: &str) {
    if let Ok(running) = running().lock() {
        for ((db, _), stop) in running.iter() {
            if db == db_path {
                stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Starts reading the tree of `message_id`'s file (`source`) in the background, unless a pass is
/// already reading that file — it completes this message too.
pub(crate) fn spawn_attachment_preparation(db_path: &str, message_id: &str, source: &str) {
    let key = (db_path.to_string(), source.to_string());
    let message_id = message_id.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let started = running()
        .lock()
        .map(|mut running| match running.entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&stop));
                true
            }
        })
        .unwrap_or(false);
    if !started {
        return;
    }
    let spawned = thread::Builder::new()
        .name("omni-attachment-prepare".to_string())
        .spawn(move || {
            let (db_path, _) = &key;
            let started = std::time::Instant::now();
            let outcome = prepare(db_path, &message_id, &stop);
            if let Ok(mut running) = running().lock() {
                running.remove(&key);
            }
            omni_app_trace_log(
                "attachment/prepare",
                &match outcome {
                    Ok(ready) => format!(
                        "message={message_id} ready in {} ms, for {} message(s): {}",
                        started.elapsed().as_millis(),
                        ready.len(),
                        ready.join(",")
                    ),
                    Err(error) => format!("message={message_id} ended: {error}"),
                },
            );
        });
    if spawned.is_err() {
        if let Ok(mut running) = running().lock() {
            running.remove(&(db_path.to_string(), source.to_string()));
        }
    }
}

/// Every file of this profile still being prepared, when no pass is at it (a restart cut it short).
pub(crate) fn resume_attachment_preparations(storage: &Storage) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().into_owned();
    for (message_id, source) in storage.list_preparing_attachments()? {
        spawn_attachment_preparation(&db_path, &message_id, &source);
    }
    Ok(())
}

/// The messages it made ready (every one waiting on this file).
fn prepare(db_path: &str, message_id: &str, stop: &AtomicBool) -> OmniResult<Vec<String>> {
    let storage = Storage::open(Path::new(db_path))?;
    let mut reported: u64 = 0;
    let mut still_preparing = true;
    let prepared = storage.finish_outgoing_attachment_preparation(
        message_id,
        &mut |read| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            if read - reported >= PROGRESS_EVERY_BYTES {
                reported = read;
                // No message left waiting on it (cancelled, the chat deleted) stops the reading.
                still_preparing = storage
                    .set_attachment_preparation_progress(message_id, read as i64, current_unix_ms())
                    .unwrap_or(true);
            }
            still_preparing
        },
        current_unix_ms(),
    );
    let ready = match prepared {
        Ok(ready) => ready,
        Err(error) => {
            // Stopped on purpose: whoever stopped it has already said why. Otherwise the file could
            // not be read (moved, changed, gone): the message fails, nothing was offered.
            if !error.to_string().contains(omni_storage::ATTACHMENT_PREPARATION_STOPPED) {
                let _ = storage.fail_outgoing_attachment_preparation(message_id, &error.to_string(), current_unix_ms());
            }
            return Err(error);
        }
    };
    // A 1:1 message goes on the queue's next tick; a group offer is sent from here.
    for ready_id in &ready {
        let Some(runtime) = storage.get_attachment_runtime_info(ready_id)? else {
            continue;
        };
        if storage.get_group_by_dialog_key(&runtime.dialog_key)?.is_some() {
            if let Err(error) = crate::app_groups::send_prepared_group_attachment_offer(&storage, ready_id) {
                omni_app_trace_log("attachment/prepare", &format!("message={ready_id} group offer not sent: {error}"));
            }
        }
    }
    Ok(ready)
}

/// Whether the message's file is still being prepared — the queue, the copies to our devices and
/// the history hand-over leave such a message be: there is nothing to offer yet.
pub(crate) fn attachment_is_preparing(storage: &Storage, message_id: &str) -> bool {
    matches!(
        storage.get_attachment_runtime_info(message_id),
        Ok(Some(runtime)) if runtime.attachment.state == AttachmentTransferState::Preparing
    )
}
