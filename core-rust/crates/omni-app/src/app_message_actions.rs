//! Deleting and pinning messages. Deleting for oneself is local: a tombstone keeps history sync from
//! bringing the line back. Deleting for everyone is its author's right alone, in a 1:1 chat kept unsent
//! until a link takes it, and it cannot reach a copy someone has already saved. A pin is anyone's in the
//! conversation; the latest by its author's clock stands.

use omni_group_mls::MemberCard;
use omni_messaging::SecurityPayloadKind;
use omni_protocol::{GroupAppKindV1, GroupAppPayloadV1};
use omni_storage::Storage;
use omni_transport::TransportManager;
use omni_types::{ChatDirection, MessageDeleteV1, MessagePinV1, OmniError, OmniResult};

use crate::{current_unix_ms, OmniApp};

const GROUP_DIALOG_PREFIX: &str = "group:";

impl OmniApp {
    /// Takes a message away here; `for_everyone` also takes it from the others, if it is ours.
    pub fn delete_message(&self, message_id: &str, for_everyone: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("delete_message")?;
        let message = self.storage.get_message(message_id)?;
        let now = current_unix_ms();
        let group_id = message.dialog_key.strip_prefix(GROUP_DIALOG_PREFIX).map(str::to_string);
        let for_everyone = for_everyone
            && match &group_id {
                Some(group_id) => line_is_of_my_account(&self.storage, group_id, message_id, &message.direction)?,
                None => message.direction == ChatDirection::Outgoing,
            };
        // The tombstone first: nothing may bring the line back once the row is gone.
        let here = |storage: &Storage| {
            storage.record_message_deletion(message_id, &message.dialog_key, now, for_everyone && group_id.is_none())?;
            storage.delete_message_here(message_id)
        };
        // In a group the others' deletion is queued with the deletion here, as one: a group keeps
        // no unsent deletion to send later, and when queuing it failed after the line was gone
        // here, it reached nobody and could not be asked again (blind audit 2026-11-11, I6; one
        // transaction — pass 13). Failed, nothing is deleted; asked again, it goes again.
        if let (true, Some(group_id)) = (for_everyone, group_id.as_deref()) {
            let event = MessageDeleteV1 { message_id: message_id.to_string(), at_unix_ms: now };
            return self.do_and_say_to_group_as_json(group_id, GroupAppKindV1::MessageDelete, &event, now, here);
        }
        here(&self.storage)?;
        if !for_everyone || group_id.is_some() {
            return Ok(());
        }
        self.send_unsent_actions_now(&message.dialog_key)
    }

    /// Pins or unpins a message for the whole conversation.
    pub fn pin_message(&self, message_id: &str, pinned: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("pin_message")?;
        let message = self.storage.get_message(message_id)?;
        let now = current_unix_ms();
        let in_group = message.dialog_key.strip_prefix(GROUP_DIALOG_PREFIX).map(str::to_string);
        match in_group {
            // Pinned here and said to the group, as one (blind audit 2026-11-11, pass 13).
            Some(group_id) => {
                let at = self.storage.pin_time_for(message_id, now)?;
                let event = MessagePinV1 { message_id: message_id.to_string(), pinned, at_unix_ms: at };
                self.do_and_say_to_group_as_json(&group_id, GroupAppKindV1::MessagePin, &event, now, |storage| {
                    storage.set_message_pin(message_id, &message.dialog_key, pinned, at, false, now).map(|_| ())
                })
            }
            None => {
                self.storage.pin_message_here(message_id, &message.dialog_key, pinned, now, true)?;
                self.send_unsent_actions_now(&message.dialog_key)
            }
        }
    }

    /// A conversation's pins and recent deletions, for its open view.
    pub fn message_actions(&self, dialog_key: &str) -> OmniResult<omni_types::MessageActionsView> {
        let _ = self.ensure_runtime_profile_active("message_actions")?;
        let dialog_key = if dialog_key.starts_with(GROUP_DIALOG_PREFIX) { dialog_key.to_string() } else { self.storage.chat_dialog_key_for(dialog_key)? };
        Ok(omni_types::MessageActionsView {
            mark: self.storage.message_actions_mark()?.unwrap_or(0),
            pinned: self.storage.pinned_messages(&dialog_key)?,
            deleted: self.storage.recent_deletions_in_dialog(&dialog_key)?,
        })
    }

    fn do_and_say_to_group_as_json<T: serde::Serialize>(
        &self,
        group_id: &str,
        kind: GroupAppKindV1,
        event: &T,
        now: i64,
        here: impl FnOnce(&Storage) -> OmniResult<()>,
    ) -> OmniResult<()> {
        let payload = GroupAppPayloadV1 {
            message_id: crate::app_groups::random_group_message_id(),
            kind,
            body: serde_json::to_string(event).map_err(|e| OmniError::Internal(e.to_string()))?,
            authored_at_unix_ms: now,
            signature_b64: None,
            reply_to: None,
            mentions: Vec::new(),
        };
        crate::app_groups::act_and_say(&self.storage, group_id, &payload, here)?;
        self.flush_group_feed();
        Ok(())
    }

    fn send_unsent_actions_now(&self, dialog_key: &str) -> OmniResult<()> {
        let Some(contact) = self.storage.resolve_contact_for_dialog(dialog_key)? else {
            return Ok(());
        };
        let Some(remote_account_id) = contact.remote_account_id.as_ref() else {
            return Ok(());
        };
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        if let Some(session) = self.storage.get_session_for_dialog(remote_account_id, dialog_key)? {
            // Not sent now (no link): it stays unsent and goes with the next link.
            let _ = send_unsent_message_actions_for_session(&self.storage, &self.transport, &session, dialog_key);
        }
        Ok(())
    }
}

/// A group line of ours, from this device or another of our account's.
fn line_is_of_my_account(storage: &Storage, group_id: &str, message_id: &str, direction: &ChatDirection) -> OmniResult<bool> {
    if *direction == ChatDirection::Outgoing {
        return Ok(true);
    }
    let Some(me) = storage.get_local_account()?.map(|a| a.descriptor.account_id.0) else {
        return Ok(false);
    };
    author_account_is(storage, group_id, message_id, &me)
}

fn author_account_is(storage: &Storage, group_id: &str, message_id: &str, account: &str) -> OmniResult<bool> {
    let Some(sender_pub) = storage.group_line_sender_pub(message_id)? else {
        return Ok(false);
    };
    Ok(storage.list_group_members(group_id)?.iter().any(|m| m.member_pub == sender_pub && m.account_id == account))
}

/// What we deleted or pinned while the link was down; a link's start calls this too. `dialog_key` is
/// where the link runs; the actions are filed under the chat's dialog.
pub(crate) fn send_unsent_message_actions_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    dialog_key: &str,
) -> OmniResult<()> {
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(());
    }
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    let mut session = session.clone();
    for deletion in storage.unsent_message_deletions(&chat_dialog_key)? {
        let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
            &session,
            &omni_types::LogicalEventEnvelopeV1::message_delete(deletion.clone()),
            SecurityPayloadKind::MessageDelete,
            current_unix_ms(),
        )?;
        storage.store_session_record(&mut outgoing.updated_session)?;
        session = outgoing.updated_session.clone();
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
        storage.mark_message_deletion_sent(&deletion.message_id)?;
    }
    for pin in storage.unsent_message_pins(&chat_dialog_key)? {
        let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
            &session,
            &omni_types::LogicalEventEnvelopeV1::message_pin(pin.clone()),
            SecurityPayloadKind::MessagePin,
            current_unix_ms(),
        )?;
        storage.store_session_record(&mut outgoing.updated_session)?;
        session = outgoing.updated_session.clone();
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
        storage.mark_message_pin_sent(&pin.message_id)?;
    }
    Ok(())
}

/// A contact takes back a line of its own: never one of ours, never one of another conversation. A line
/// not here yet is taken back when it comes — if it is the contact's ([`apply_pending_message_deletes`]).
pub(crate) fn receive_message_delete(storage: &Storage, dialog_key: &str, event: &MessageDeleteV1) -> OmniResult<()> {
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    if !storage.message_exists(&event.message_id)? {
        return storage.record_message_delete_request(&event.message_id, &chat_dialog_key, "", "", event.at_unix_ms, current_unix_ms());
    }
    let message = storage.get_message(&event.message_id)?;
    if message.dialog_key != chat_dialog_key || message.direction != ChatDirection::Incoming {
        return Ok(());
    }
    storage.record_message_deletion(&event.message_id, &chat_dialog_key, event.at_unix_ms, false)?;
    storage.delete_message_here(&event.message_id)
}

/// An incoming line `message_id` for `dialog_key` names a message that is already here in another
/// chat, or one of ours: the incoming one must not touch it. The id is the sender's, and a contact or
/// a group member who knew another chat's id used to re-point its file, its sender, or attach a file
/// to it (security review 2026-11-09, F2). The same line of the same chat again is a resend — not
/// refused here; what is stored for it does not change.
pub(crate) fn message_id_taken_elsewhere(storage: &Storage, message_id: &str, dialog_key: &str) -> OmniResult<bool> {
    if !storage.message_exists(message_id)? {
        return Ok(false);
    }
    let message = storage.get_message(message_id)?;
    let taken = message.direction != ChatDirection::Incoming || message.dialog_key != storage.chat_dialog_key_for(dialog_key)?;
    if taken {
        crate::omni_app_trace_log("recv/message-id", &format!("message={message_id} is another chat's or ours: incoming copy dropped"));
    }
    Ok(taken)
}

/// May the one who asked delete `message_id` (here now)? The same rule as for a line that was here when
/// the deletion came: in a 1:1 chat the contact, its own lines only; in a group the line's author — by
/// its card or its account — or another device of ours, our own lines.
fn may_delete(storage: &Storage, message_id: &str, dialog_key: &str, requester_pub: &str, requester_account: &str) -> OmniResult<bool> {
    let message = storage.get_message(message_id)?;
    if message.dialog_key != dialog_key {
        return Ok(false);
    }
    let Some(group_id) = dialog_key.strip_prefix(GROUP_DIALOG_PREFIX) else {
        return Ok(requester_pub.is_empty() && message.direction == ChatDirection::Incoming);
    };
    let by_card = !requester_pub.is_empty() && storage.group_line_sender_pub(message_id)?.as_deref() == Some(requester_pub);
    let by_account = !requester_account.is_empty() && author_account_is(storage, group_id, message_id, requester_account)?;
    let from_my_other_device = message.direction == ChatDirection::Outgoing
        && !requester_account.is_empty()
        && storage.get_local_account()?.is_some_and(|a| a.descriptor.account_id.0 == requester_account);
    Ok(by_card || by_account || from_my_other_device)
}

/// A line just came: a deletion asked for it before it was here takes it back now, if the one who
/// asked may (`may_delete`). A deletion by anyone else is dropped — it used to stand as a tombstone,
/// so a member who knew a line's id could keep it from those it had not reached yet (audit
/// 2026-11-09, F07). Call after the line is stored with its author.
pub(crate) fn apply_pending_message_deletes(storage: &Storage, message_id: &str) -> OmniResult<bool> {
    let requests = storage.take_message_delete_requests(message_id)?;
    if requests.is_empty() || !storage.message_exists(message_id)? {
        return Ok(false);
    }
    for (dialog_key, requester_pub, requester_account, at_unix_ms) in requests {
        if may_delete(storage, message_id, &dialog_key, &requester_pub, &requester_account)? {
            storage.record_message_deletion(message_id, &dialog_key, at_unix_ms, false)?;
            storage.delete_message_here(message_id)?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn receive_message_pin(storage: &Storage, dialog_key: &str, event: &MessagePinV1) -> OmniResult<()> {
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    storage.set_message_pin(&event.message_id, &chat_dialog_key, event.pinned, event.at_unix_ms, false, current_unix_ms())?;
    Ok(())
}

/// What our own device deleted in a chat: the same lines go here, and sync stops handing them over.
pub(crate) fn receive_own_device_deletions(storage: &Storage, message_ids: &[String], now_unix_ms: i64) -> OmniResult<()> {
    for id in message_ids {
        if storage.message_is_deleted(id)? {
            continue;
        }
        if storage.message_exists(id)? {
            let message = storage.get_message(id)?;
            storage.record_message_deletion(id, &message.dialog_key, now_unix_ms, false)?;
            storage.delete_message_here(id)?;
        } else {
            // Not here yet: the tombstone all the same (our own device says so), so a copy that
            // comes later — from a device behind on sync — does not bring the line back. Nothing was
            // kept, and the line came back (blind audit 2026-11-13, pass 22).
            storage.record_message_deletion(id, "", now_unix_ms, false)?;
        }
    }
    Ok(())
}

/// A member takes back a line of their own; the card says who is asking.
pub(crate) fn receive_group_delete(storage: &Storage, dialog_key: &str, sender: &MemberCard, payload: &GroupAppPayloadV1) -> OmniResult<()> {
    let event: MessageDeleteV1 = serde_json::from_str(&payload.body).map_err(|e| OmniError::InvalidInput(format!("group delete: {e}")))?;
    omni_protocol::validate_message_delete_v1(&event)?;
    if !storage.message_exists(&event.message_id)? {
        return storage.record_message_delete_request(
            &event.message_id,
            dialog_key,
            &sender.member_pub,
            &sender.account_id,
            event.at_unix_ms,
            current_unix_ms(),
        );
    }
    if !may_delete(storage, &event.message_id, dialog_key, &sender.member_pub, &sender.account_id)? {
        return Ok(());
    }
    storage.record_message_deletion(&event.message_id, dialog_key, event.at_unix_ms, false)?;
    storage.delete_message_here(&event.message_id)
}

pub(crate) fn receive_group_pin(storage: &Storage, dialog_key: &str, payload: &GroupAppPayloadV1) -> OmniResult<()> {
    let event: MessagePinV1 = serde_json::from_str(&payload.body).map_err(|e| OmniError::InvalidInput(format!("group pin: {e}")))?;
    omni_protocol::validate_message_pin_v1(&event)?;
    storage.set_message_pin(&event.message_id, dialog_key, event.pinned, event.at_unix_ms, false, current_unix_ms())?;
    Ok(())
}
