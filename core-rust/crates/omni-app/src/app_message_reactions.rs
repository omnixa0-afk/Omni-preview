//! Emoji reactions on messages (Telegram's). One reaction per person on a message; a later one replaces
//! it, an empty one takes it away, and the latest by its author's clock wins, so a late or repeated event
//! cannot bring an old one back. In a group it goes through the group, the reactor known by the
//! authenticated card; in a 1:1 chat as an encrypted event, kept unsent until a link takes it.

use omni_group_mls::MemberCard;
use omni_messaging::SecurityPayloadKind;
use omni_protocol::{GroupAppKindV1, GroupAppPayloadV1};
use omni_storage::Storage;
use omni_transport::TransportManager;
use omni_types::{OmniError, OmniResult, ReactionV1};

use crate::{current_unix_ms, OmniApp};

const GROUP_DIALOG_PREFIX: &str = "group:";

impl OmniApp {
    /// Our reaction to a message of any conversation; empty `emoji` takes it away.
    pub fn react_to_message(&self, message_id: &str, emoji: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("react_to_message")?;
        let reaction = ReactionV1 { message_id: message_id.to_string(), emoji: emoji.to_string(), at_unix_ms: current_unix_ms() };
        omni_protocol::validate_reaction_v1(&reaction)?;
        let message = self.storage.get_message(message_id)?;
        let me = self
            .storage
            .get_local_account()?
            .map(|a| a.descriptor.account_id.0)
            .ok_or_else(|| OmniError::Internal("no local account".into()))?;
        let set_here = |storage: &Storage| storage.set_message_reaction(message_id, &message.dialog_key, &me, true, emoji, reaction.at_unix_ms, true, reaction.at_unix_ms).map(|_| ());

        if let Some(group_id) = message.dialog_key.strip_prefix(GROUP_DIALOG_PREFIX) {
            let payload = GroupAppPayloadV1 {
                message_id: crate::app_groups::random_group_message_id(),
                kind: GroupAppKindV1::Reaction,
                body: serde_json::to_string(&reaction).map_err(|e| OmniError::Internal(e.to_string()))?,
                authored_at_unix_ms: reaction.at_unix_ms,
                signature_b64: None,
                reply_to: None,
                mentions: Vec::new(),
            };
            // Set here, said to the group and marked as said, as one (blind audit 2026-11-11, pass 13).
            crate::app_groups::act_and_say(&self.storage, group_id, &payload, |storage| {
                set_here(storage)?;
                storage.mark_reaction_sent(message_id, &me).map(|_| ())
            })?;
            self.flush_group_feed();
            return Ok(());
        }
        set_here(&self.storage)?;

        let Some(contact) = self.storage.resolve_contact_for_dialog(&message.dialog_key)? else {
            return Ok(());
        };
        let Some(remote_account_id) = contact.remote_account_id.as_ref() else {
            return Ok(());
        };
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        if let Some(session) = self.storage.get_session_for_dialog(remote_account_id, &message.dialog_key)? {
            // Not sent now (no link): it stays unsent and goes with the next link.
            let _ = send_unsent_reactions_for_session(&self.storage, &self.transport, &session, &message.dialog_key);
        }
        Ok(())
    }
}

/// What we reacted to while the link was down; a link's start calls this too.
pub(crate) fn send_unsent_reactions_for_session(
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
    for (reactor, reaction) in storage.unsent_reactions(&chat_dialog_key)? {
        let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
            &session,
            &omni_types::LogicalEventEnvelopeV1::reaction(reaction.clone()),
            SecurityPayloadKind::Reaction,
            current_unix_ms(),
        )?;
        storage.store_session_record(&mut outgoing.updated_session)?;
        session = outgoing.updated_session.clone();
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
        storage.mark_reaction_sent(&reaction.message_id, &reactor)?;
    }
    Ok(())
}

/// A contact's reaction: only to a message of its own dialog.
pub(crate) fn receive_reaction(storage: &Storage, dialog_key: &str, reactor: &str, reaction: &ReactionV1) -> OmniResult<()> {
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    storage.set_message_reaction(&reaction.message_id, &chat_dialog_key, reactor, false, &reaction.emoji, reaction.at_unix_ms, false, crate::current_unix_ms())?;
    Ok(())
}

/// A group member's reaction, or our own from another device.
pub(crate) fn receive_group_reaction(storage: &Storage, dialog_key: &str, sender: &MemberCard, payload: &GroupAppPayloadV1) -> OmniResult<()> {
    let reaction: ReactionV1 = serde_json::from_str(&payload.body).map_err(|e| OmniError::InvalidInput(format!("group reaction: {e}")))?;
    omni_protocol::validate_reaction_v1(&reaction)?;
    let mine = storage.get_local_account()?.is_some_and(|a| a.descriptor.account_id.0 == sender.account_id);
    storage.set_message_reaction(&reaction.message_id, dialog_key, &sender.account_id, mine, &reaction.emoji, reaction.at_unix_ms, false, crate::current_unix_ms())?;
    Ok(())
}
