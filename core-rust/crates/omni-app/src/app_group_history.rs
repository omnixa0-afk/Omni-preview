// Recent history for a newcomer (docs/decisions/group-history-sync.md; owner decisions 2026-10-11:
// on by default, an admin can turn it off; every line signed by its own author).
//
// MLS keeps a newcomer out of everything said before it joined: the keys of past epochs were never
// its. So the member that adds it hands it the last lines itself, inside the new epoch, as one
// application message (`GroupAppKindV1::History`). A member could put anything into such a packet —
// so a line travels only with its author's own signature over the line (`group_line_signing_bytes`),
// made when the author sent it. The newcomer takes a line only if that signature holds for the key it
// names; the member handing the packet over can leave lines out, never change or invent one.
//
// Lines from before this build carry no signature and never travel. Files travel as their offers:
// the newcomer pulls the bytes from the author, as any member does.

use base64::Engine as _;
use omni_group_mls::MemberCard;
use omni_protocol::{
    group_line_signing_bytes, GroupAppKindV1, GroupAppPayloadV1, GroupHistoryLineV1, GroupHistoryPacketV1,
    GROUP_HISTORY_MAX_BYTES, GROUP_HISTORY_MAX_LINES,
};
use omni_storage::{GroupMessageMeta, GroupRecord, GroupSignedLine, Storage};
use omni_types::{OmniError, OmniResult};

use crate::omni_app_trace_log;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn kind_name(kind: GroupAppKindV1) -> Option<&'static str> {
    match kind {
        GroupAppKindV1::Text => Some("text"),
        GroupAppKindV1::Attachment => Some("attachment"),
        _ => None,
    }
}

fn kind_of(name: &str) -> Option<GroupAppKindV1> {
    match name {
        "text" => Some(GroupAppKindV1::Text),
        "attachment" => Some(GroupAppKindV1::Attachment),
        _ => None,
    }
}

/// Sign our own line before it goes out, and keep it for later newcomers.
pub(crate) fn sign_own_line(storage: &Storage, group_id: &str, payload: &mut GroupAppPayloadV1, me: &MemberCard) -> OmniResult<()> {
    let Some(kind) = kind_name(payload.kind) else {
        return Ok(());
    };
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::Internal("no local account".into()))?;
    let identity = storage
        .get_local_identity_key(&account.descriptor.account_id)?
        .ok_or_else(|| OmniError::Internal("no identity key".into()))?;
    let bytes = group_line_signing_bytes(group_id, &payload.message_id, payload.kind, &payload.body, payload.authored_at_unix_ms, payload.reply_to.as_deref(), &payload.mentions);
    let signature = omni_crypto::sign_message(&identity.protected_signing_private_key, &bytes)?;
    let signature_b64 = B64.encode(&signature.bytes);
    payload.signature_b64 = Some(signature_b64.clone());
    storage.record_group_signed_line(
        group_id,
        &GroupSignedLine {
            message_id: payload.message_id.clone(),
            kind: kind.to_string(),
            sender_pub: me.member_pub.clone(),
            sender_name: me.display_name.clone(),
            body: payload.body.clone(),
            authored_at_unix_ms: payload.authored_at_unix_ms,
            signature_b64,
            reply_to: payload.reply_to.clone(),
            mentions: payload.mentions.clone(),
        },
    )
}

/// Whether `signature_b64` is `sender_pub`'s signature over the line.
fn line_signature_holds(
    group_id: &str,
    message_id: &str,
    kind: GroupAppKindV1,
    body: &str,
    authored_at_unix_ms: i64,
    reply_to: Option<&str>,
    mentions: &[String],
    sender_pub: &str,
    signature_b64: &str,
) -> bool {
    let Some(key) = B64
        .decode(sender_pub)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .map(omni_crypto::IdentitySigningPublicKey)
    else {
        return false;
    };
    let Ok(signature) = B64.decode(signature_b64) else {
        return false;
    };
    let bytes = group_line_signing_bytes(group_id, message_id, kind, body, authored_at_unix_ms, reply_to, mentions);
    omni_crypto::verify_signature(&key, &bytes, &omni_crypto::Signature { bytes: signature }).is_ok()
}

/// A member's line arrived: keep its signature if it holds, so the line can go to later newcomers.
/// `false` when it carries a signature that does not hold (a broken or forged line: dropped).
pub(crate) fn keep_member_line(storage: &Storage, group_id: &str, sender: &MemberCard, payload: &GroupAppPayloadV1) -> bool {
    let (Some(kind), Some(signature_b64)) = (kind_name(payload.kind), payload.signature_b64.as_deref()) else {
        return true; // unsigned (an older build): shown, never forwarded
    };
    if !line_signature_holds(
        group_id,
        &payload.message_id,
        payload.kind,
        &payload.body,
        payload.authored_at_unix_ms,
        payload.reply_to.as_deref(),
        &payload.mentions,
        &sender.member_pub,
        signature_b64,
    ) {
        omni_app_trace_log("group/history", &format!("group={group_id} line {} with a bad signature dropped", payload.message_id));
        return false;
    }
    // Shown, but not kept for newcomers when dated past what a newcomer takes: the packet is the
    // latest lines by that date, and a member's lines dated i64::MAX filled it — the newcomer dropped
    // them all and got none of the real history (blind audit 2026-11-12, pass 15).
    if payload.authored_at_unix_ms > crate::current_unix_ms() + HISTORY_CLOCK_SKEW_MS {
        omni_app_trace_log("group/history", &format!("group={group_id} line {} dated ahead not kept for newcomers", payload.message_id));
        return true;
    }
    if let Err(error) = storage.record_group_signed_line(
        group_id,
        &GroupSignedLine {
            message_id: payload.message_id.clone(),
            kind: kind.to_string(),
            sender_pub: sender.member_pub.clone(),
            sender_name: sender.display_name.clone(),
            body: payload.body.clone(),
            authored_at_unix_ms: payload.authored_at_unix_ms,
            signature_b64: signature_b64.to_string(),
            reply_to: payload.reply_to.clone(),
            mentions: payload.mentions.clone(),
        },
    ) {
        // Shown all the same; only newcomers will not get it. Said here, not swallowed (pass 16).
        omni_app_trace_log("group/history", &format!("group={group_id} line {} not kept for newcomers: {error}", payload.message_id));
    }
    true
}

/// The packet for a newcomer: the latest signed lines (only `only_from`'s, if given), oldest first,
/// as many as fit.
fn history_packet(storage: &Storage, group_id: &str, only_from: Option<&str>) -> OmniResult<Option<String>> {
    let mut lines: Vec<GroupHistoryLineV1> = storage
        .list_recent_group_signed_lines(group_id, GROUP_HISTORY_MAX_LINES, only_from)?
        .into_iter()
        .filter_map(|line| {
            Some(GroupHistoryLineV1 {
                kind: kind_of(&line.kind)?,
                message_id: line.message_id,
                sender_pub: line.sender_pub,
                sender_name: line.sender_name,
                body: line.body,
                authored_at_unix_ms: line.authored_at_unix_ms,
                signature_b64: line.signature_b64,
                reply_to: line.reply_to,
                mentions: line.mentions,
            })
        })
        .collect();
    lines.reverse();
    loop {
        if lines.is_empty() {
            return Ok(None);
        }
        let json = serde_json::to_string(&GroupHistoryPacketV1 { lines: lines.clone() })
            .map_err(|e| OmniError::Internal(e.to_string()))?;
        if json.len() <= GROUP_HISTORY_MAX_BYTES {
            return Ok(Some(json));
        }
        // Too big: the oldest lines give way.
        let drop = (lines.len() / 10).max(1);
        lines.drain(..drop);
    }
}

/// We just added a newcomer: hand it the recent history, unless an admin turned that off. Sent in
/// the new epoch, which the newcomer has the keys of; the members already in it ignore what they
/// have.
pub(crate) fn send_newcomer_history(storage: &Storage, group_id: &str, newcomer_history: bool) -> OmniResult<()> {
    if !newcomer_history {
        omni_app_trace_log("group/history", &format!("group={group_id} newcomer history is off"));
        return Ok(());
    }
    send_history(storage, group_id, None)
}

/// Hand the group the recent signed lines (only `only_from`'s, if given): to a newcomer, to a member
/// put back after its branch lost, or — from that member — what it said on the losing branch.
pub(crate) fn send_history(storage: &Storage, group_id: &str, only_from: Option<&str>) -> OmniResult<()> {
    let Some(body) = history_packet(storage, group_id, only_from)? else {
        return Ok(());
    };
    let payload = GroupAppPayloadV1 {
        message_id: crate::app_groups::random_group_message_id(),
        kind: GroupAppKindV1::History,
        body,
        authored_at_unix_ms: crate::current_unix_ms(),
        signature_b64: None,
        reply_to: None,
        mentions: Vec::new(),
    };
    crate::app_groups::send_app_payload(storage, group_id, &payload)?;
    omni_app_trace_log("group/history", &format!("group={group_id} history sent ({} bytes)", payload.body.len()));
    Ok(())
}

/// How far ahead of our clock a history line may be dated.
const HISTORY_CLOCK_SKEW_MS: i64 = 10 * 60 * 1000;

/// A history packet arrived. A line counts only if its author is a member we know in this group and
/// its signature holds, and it is not dated in the future; it is shown under the name the member's
/// own card gives, not the one the line carries (V-14: any member could hand a newcomer lines signed
/// with a key of its own under another member's name, at any time). A line of someone who left
/// before we came is refused with them — we cannot tell they were ever in. A line we have is
/// skipped, our own are skipped. Returns how many lines were added.
pub(crate) fn receive_history(storage: &Storage, group_id: &str, group: &GroupRecord, payload: &GroupAppPayloadV1) -> OmniResult<usize> {
    let packet: GroupHistoryPacketV1 = serde_json::from_str(&payload.body)
        .map_err(|e| OmniError::InvalidInput(format!("group history: {e}")))?;
    omni_protocol::validate_group_history_packet_v1(&packet)?;
    let members: std::collections::HashMap<String, String> = storage
        .list_group_members(group_id)?
        .into_iter()
        .map(|member| (member.member_pub, member.display_name))
        .collect();
    let latest = crate::current_unix_ms() + HISTORY_CLOCK_SKEW_MS;
    let mut added = 0;
    let mut refused = 0;
    for line in &packet.lines {
        if line.sender_pub == group.my_member_pub || storage.message_exists(&line.message_id)? {
            continue;
        }
        let Some(member_name) = members.get(&line.sender_pub) else {
            refused += 1;
            continue;
        };
        if line.authored_at_unix_ms > latest {
            refused += 1;
            continue;
        }
        if !line_signature_holds(
            group_id,
            &line.message_id,
            line.kind,
            &line.body,
            line.authored_at_unix_ms,
            line.reply_to.as_deref(),
            &line.mentions,
            &line.sender_pub,
            &line.signature_b64,
        ) {
            refused += 1;
            continue;
        }
        let sender = MemberCard {
            member_pub: line.sender_pub.clone(),
            account_id: String::new(),
            display_name: member_name.clone(),
            device_chain: String::new(),
        };
        let as_payload = GroupAppPayloadV1 {
            message_id: line.message_id.clone(),
            kind: line.kind,
            body: line.body.clone(),
            authored_at_unix_ms: line.authored_at_unix_ms,
            signature_b64: Some(line.signature_b64.clone()),
            reply_to: line.reply_to.clone(),
            mentions: line.mentions.clone(),
        };
        // Deleted here: no one's history brings it back.
        if storage.message_is_deleted(&line.message_id).unwrap_or(false) {
            continue;
        }
        let taken = match line.kind {
            GroupAppKindV1::Text => storage
                .insert_incoming_message(
                    &line.message_id,
                    &group.dialog_key,
                    &line.body,
                    crate::app_groups::shown_line_time(line.authored_at_unix_ms, crate::current_unix_ms()),
                )
                .and_then(|_| storage.remember_received_message_reply(&line.message_id, line.reply_to.as_deref()))
                .and_then(|_| storage.remember_message_mentions(&line.message_id, &line.mentions))
                .and_then(|_| {
                    // A line of the past: not a new line for an agent's read place.
                    storage.insert_group_history_message_meta(
                        group_id,
                        &GroupMessageMeta {
                            message_id: line.message_id.clone(),
                            sender_pub: Some(line.sender_pub.clone()),
                            sender_name: member_name.clone(),
                            kind: "text".into(),
                        },
                    )
                }),
            GroupAppKindV1::Attachment => crate::app_group_attachments::receive_offer(storage, group_id, group, &sender, &as_payload),
            _ => continue,
        };
        match taken {
            Ok(()) => {
                // Its author took it back before it came here (a deletion that waited for it).
                if crate::app_message_actions::apply_pending_message_deletes(storage, &line.message_id).unwrap_or(false) {
                    continue;
                }
                // The group's past handed to a newcomer is not news: it does not light the counter.
                let _ = storage.mark_dialog_read_through_history_line(&group.dialog_key, line.authored_at_unix_ms, &line.message_id);
                keep_member_line(storage, group_id, &sender, &as_payload);
                added += 1;
            }
            Err(error) => omni_app_trace_log("group/history", &format!("line {} not taken: {error}", line.message_id)),
        }
    }
    omni_app_trace_log(
        "group/history",
        &format!("group={group_id} history: +{added} line(s), {refused} refused (unknown author, bad signature or dated ahead)"),
    );
    Ok(added)
}
