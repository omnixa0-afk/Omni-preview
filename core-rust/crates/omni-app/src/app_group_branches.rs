// A group that went on in two branches comes back to one (docs/decisions/group-history-sync.md,
// «Синхронизация эпох»).
//
// A network split, or two devices of one account apart, and both halves of a group keep changing it:
// each half is its own chain of MLS epochs. One epoch apart the engine rolls the loser back itself.
// Deeper than that it only tells (`Inbound::ForkSeen`): which branch wins is decided at the epoch they
// split, by the rank of the Commit there — the same rule everywhere, nothing to negotiate.
//
// - A device whose branch lost waits a moment (earlier Commits of the other branch may still be on
//   the way and move the split point back), then asks to be put back: a fresh KeyPackage on the
//   group's feed, again every ten minutes until it is back.
// - A member of the winning branch puts it back — its old leaf out, the KeyPackage in, one Commit
//   (`Engine::readd_member`) — and lays the Welcome on the feed. Members take turns by key order,
//   ten minutes apart, so the first one does it and the others see it done.
// - Back from the Welcome, the device re-makes its own changes of the losing branch on the winning
//   one — who it added or removed (an admin's removal stays a removal); the winning branch's name,
//   picture and admins stand — and hands the group what it said there, signed by itself.
//   The member that put it back hands it the winning branch's recent lines.
//
// Nothing local is erased: the losing branch's lines stay in the chat. A device that cannot get back
// (nobody of the other branch around) keeps working on its own branch and keeps asking.

use omni_group_mls::MemberCard;
use omni_storage::{GroupPendingWelcome, GroupReaddRequest, Storage};
use omni_types::OmniResult;

use crate::app_groups::{
    apply_view, group_id_bytes, insert_system_line, local_mls_identity, own_commit_bookkeeping, reissue_change, with_engine,
    OwnChange,
};
use crate::omni_app_trace_log;

/// Commits of the other branch keep arriving for a while; the earliest epoch seen decides.
const FORK_SETTLE_MS: i64 = 20_000;

/// A device whose branch lost asks again this often.
const REJOIN_REQUEST_EVERY_MS: i64 = 10 * 60 * 1000;

/// Members of the winning branch take turns putting a member back, this far apart: the first in key
/// order does it at once, the next only if it is still not done ten minutes later (the first was away).
const READD_TURN_MS: i64 = 10 * 60 * 1000;

/// A Welcome waits this long after its Commit, so a competing Commit for the same epoch shows up.
const WELCOME_HOLD_MS: i64 = 30_000;

/// A member was put back (by us, or by a Commit we merged): what it asked is done.
pub(crate) fn note_put_back(storage: &Storage, group_id: &str, member_pub: &str) {
    let _ = storage.drop_group_readd_request(group_id, member_pub);
}

pub(crate) fn on_fork_seen(storage: &Storage, group_id: &str, epoch: u64, ours_wins: bool, now: i64) -> OmniResult<()> {
    storage.note_group_fork(group_id, epoch, ours_wins, now)?;
    omni_app_trace_log("group/branch", &format!("group={group_id} another branch since epoch {epoch}, ours wins: {ours_wins}"));
    Ok(())
}

pub(crate) fn on_rejoin_request(
    storage: &Storage,
    group_id: &str,
    card: &MemberCard,
    key_package: &[u8],
    now: i64,
) -> OmniResult<()> {
    storage.record_group_readd_request(&GroupReaddRequest {
        group_id: group_id.to_string(),
        member_pub: card.member_pub.clone(),
        key_package: key_package.to_vec(),
        seen_at_unix_ms: now,
    })?;
    omni_app_trace_log("group/branch", &format!("group={group_id} {} asks to be put back", card.display_name));
    Ok(())
}

/// A Welcome on the feed: ours only while we wait to be put back, and only if it opens for us.
pub(crate) fn on_welcome(
    storage: &Storage,
    transport: &crate::TransportManager,
    group_id: &str,
    welcome: &[u8],
    now: i64,
) -> OmniResult<()> {
    let Some(state) = storage.get_group_branch_state(group_id)? else {
        return Ok(());
    };
    if state.rejoining_since_unix_ms.is_none() {
        return Ok(());
    }
    let joined = with_engine(storage, |engine| engine.rejoin(welcome));
    let Ok(joined) = joined else {
        return Ok(()); // a Welcome for someone else
    };
    if joined != group_id_bytes(group_id)? {
        return Ok(());
    }
    let gid = group_id_bytes(group_id)?;
    let view = with_engine(storage, |engine| engine.view(&gid))?;
    apply_view(storage, group_id, &view, now)?;
    storage.clear_group_branch_state(group_id)?;
    // Requests we kept while out are from before we came back; a member still out asks again.
    storage.drop_group_readd_requests(group_id)?;
    insert_system_line(storage, group_id, "Ветки группы сведены: вы снова вместе со всеми", now)?;
    // Messages of the winning branch that came before the Welcome open now.
    for pending in storage.take_group_pending_inbound(group_id)? {
        let _ = crate::app_groups::receive_mls(storage, transport, group_id, &pending);
    }
    omni_app_trace_log("group/branch", &format!("group={group_id} back on the winning branch at epoch {}", view.epoch));
    // Who we added or removed on the losing branch, made again on this one (a removal stays a
    // removal). The name, picture, admins and switches of the winning branch stand: ours were set
    // apart from what the others did since, and replaying them would undo newer changes.
    for (_, json) in storage.take_group_own_changes_after(group_id, state.fork_epoch)? {
        let Ok(change) = serde_json::from_str::<OwnChange>(&json) else {
            continue;
        };
        if matches!(change, OwnChange::Add { .. } | OwnChange::Remove { .. }) {
            if let Err(error) = reissue_change(storage, transport, group_id, change) {
                omni_app_trace_log("group/branch", &format!("group={group_id} a change not made again: {error}"));
            }
        }
    }
    // What we said there, for the others (each line signed by us).
    let (_, me) = local_mls_identity(storage)?;
    crate::app_group_history::send_history(storage, group_id, Some(&me.member_pub))?;
    crate::record_group_changed_event(&storage.db_path().to_string_lossy(), group_id);
    Ok(())
}

/// One round over every group that went on in two branches. Runs on the mesh tick.
pub(crate) fn group_branch_tick(storage: &Storage, now: i64) {
    for state in storage.list_group_branch_states().unwrap_or_default() {
        if let Err(error) = branch_round(storage, &state, now) {
            omni_app_trace_log("group/branch", &format!("group={} round failed: {error}", state.group_id));
        }
    }
    for pending in storage.list_group_pending_welcomes().unwrap_or_default() {
        if let Err(error) = welcome_round(storage, &pending, now) {
            omni_app_trace_log("group/branch", &format!("group={} welcome not sent: {error}", pending.group_id));
            let _ = storage.drop_group_pending_welcome(&pending.group_id, &pending.member_pub);
        }
    }
    for request in storage.list_group_readd_requests().unwrap_or_default() {
        if let Err(error) = readd_round(storage, &request, now) {
            omni_app_trace_log("group/branch", &format!("group={} put back failed: {error}", request.group_id));
            let _ = storage.drop_group_readd_request(&request.group_id, &request.member_pub);
        }
    }
}

fn branch_round(storage: &Storage, state: &omni_storage::GroupBranchState, now: i64) -> OmniResult<()> {
    if storage.get_group(&state.group_id)?.is_none_or(|group| group.my_status != "member") {
        return storage.clear_group_branch_state(&state.group_id);
    }
    if state.rejoining_since_unix_ms.is_none() {
        if now - state.first_seen_at_unix_ms < FORK_SETTLE_MS {
            return Ok(());
        }
        if state.ours_wins {
            // The others come to us: nothing to do here.
            return storage.clear_group_branch_state(&state.group_id);
        }
        storage.mark_group_rejoining(&state.group_id, now)?;
        insert_system_line(
            storage,
            &state.group_id,
            "Группа разошлась: часть участников жила отдельно. Сводим ветки — ваши сообщения не потеряются",
            now,
        )?;
        crate::record_group_changed_event(&storage.db_path().to_string_lossy(), &state.group_id);
    }
    if state.requested_at_unix_ms != 0 && now - state.requested_at_unix_ms < REJOIN_REQUEST_EVERY_MS {
        return Ok(());
    }
    // One KeyPackage for the whole wait: a copy of the request still travelling once we are back
    // names the leaf we came back with, and is dropped for that (`Engine::has_leaf_of`).
    let request = match &state.request_mls {
        Some(request) => request.clone(),
        None => with_engine(storage, |engine| engine.rejoin_request())?,
    };
    storage.enqueue_group_feed(&state.group_id, &request, now)?;
    storage.mark_group_rejoin_requested(&state.group_id, now, &request)?;
    omni_app_trace_log("group/branch", &format!("group={} asked to be put back", state.group_id));
    Ok(())
}

/// Send a held Welcome once our Commit has stood a while; drop it if another Commit won its epoch.
fn welcome_round(storage: &Storage, pending: &GroupPendingWelcome, now: i64) -> OmniResult<()> {
    if now - pending.created_at_unix_ms < WELCOME_HOLD_MS {
        return Ok(());
    }
    let gid = group_id_bytes(&pending.group_id)?;
    let merged = with_engine(storage, |engine| engine.branch_rank(&gid, pending.epoch_before))?;
    storage.drop_group_pending_welcome(&pending.group_id, &pending.member_pub)?;
    if merged.map(|rank| rank.to_vec()) != Some(pending.commit_rank.clone()) {
        omni_app_trace_log("group/branch", &format!("group={} our put-back lost its epoch: welcome dropped", pending.group_id));
        return Ok(());
    }
    storage.enqueue_group_feed(&pending.group_id, &pending.welcome, now)?;
    omni_app_trace_log("group/branch", &format!("group={} welcome out", pending.group_id));
    // What was said on this branch, for the member coming back — after the Welcome, in an epoch it
    // will hold the keys of.
    crate::app_group_history::send_history(storage, &pending.group_id, None)?;
    Ok(())
}

/// Put a member back when our turn comes (or forget the request if somebody did).
fn readd_round(storage: &Storage, request: &GroupReaddRequest, now: i64) -> OmniResult<()> {
    let group_id = &request.group_id;
    let Some(group) = storage.get_group(group_id)? else {
        return storage.drop_group_readd_request(group_id, &request.member_pub);
    };
    // A device waiting to be put back itself is on the losing branch: not its turn.
    let rejoining = storage.get_group_branch_state(group_id)?.is_some_and(|state| state.rejoining_since_unix_ms.is_some());
    if group.my_status != "member" || rejoining {
        return Ok(());
    }
    let gid = group_id_bytes(group_id)?;
    if with_engine(storage, |engine| engine.has_leaf_of(&gid, &request.key_package))? {
        return storage.drop_group_readd_request(group_id, &request.member_pub);
    }
    let view = with_engine(storage, |engine| engine.view(&gid))?;
    let mut turn: Vec<&str> = view
        .members
        .iter()
        .map(|member| member.card.member_pub.as_str())
        .filter(|member| *member != request.member_pub)
        .collect();
    turn.sort_unstable();
    let position = turn.iter().position(|member| *member == group.my_member_pub).unwrap_or(turn.len());
    if now - request.seen_at_unix_ms < position as i64 * READD_TURN_MS {
        return Ok(());
    }
    let out = with_engine(storage, |engine| engine.readd_member(&gid, &request.key_package))?;
    own_commit_bookkeeping(storage, group_id, &out.commit, &OwnChange::PutBack { member_pub: request.member_pub.clone() })?;
    // The Welcome waits until our Commit stands: two members putting the same one back at once make
    // two Commits for one epoch, and only the winner's Welcome leads into the group.
    storage.hold_group_welcome(&GroupPendingWelcome {
        group_id: group_id.clone(),
        member_pub: request.member_pub.clone(),
        epoch_before: view.epoch,
        commit_rank: omni_group_mls::commit_rank(&out.commit).to_vec(),
        welcome: out.welcome.clone(),
        created_at_unix_ms: now,
    })?;
    note_put_back(storage, group_id, &request.member_pub);
    let name = out.added.first().map(|card| card.display_name.clone()).unwrap_or_default();
    omni_app_trace_log("group/branch", &format!("group={group_id} put {name} back"));
    Ok(())
}
