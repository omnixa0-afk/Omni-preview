// Standing remote access between us and a contact (docs/decisions/remote-access-for-contacts.md).
// This file is the state plane only: the owner's grant per contact, and what a contact told us it
// lets us do. No session, no input — starting a session lives elsewhere, by design, so the "who is
// allowed" decision is small, auditable, and separate from the mechanism that acts on it.

use omni_identity::{ContactId, TrustState};
use omni_storage::ContactRemoteAccessRecord;
use omni_types::{OmniError, OmniResult, RemoteAccessMode};

use crate::{current_unix_ms, OmniApp};

impl OmniApp {
    /// Remote access with one contact, both directions (for the contact card).
    pub fn get_contact_remote_access(&self, contact_id: &ContactId) -> OmniResult<ContactRemoteAccessRecord> {
        let _ = self.ensure_runtime_profile_active("get_contact_remote_access")?;
        self.storage.get_contact_remote_access(contact_id)
    }

    /// Every contact we have remote access with, either direction (the Settings list).
    pub fn list_contact_remote_access(&self) -> OmniResult<Vec<ContactRemoteAccessRecord>> {
        let _ = self.ensure_runtime_profile_active("list_contact_remote_access")?;
        self.storage.list_contact_remote_access()
    }

    /// Let this contact control our computer (`Some(mode)`) or stop (`None`). Only a verified contact
    /// may be granted — an unverified key could be an impostor's, and a key swap at first contact
    /// would otherwise become access to this computer. Revoking is always allowed.
    pub fn set_contact_remote_access_granted(
        &self,
        contact_id: &ContactId,
        mode: Option<RemoteAccessMode>,
    ) -> OmniResult<ContactRemoteAccessRecord> {
        let _ = self.ensure_runtime_profile_active("set_contact_remote_access_granted")?;
        if mode.is_some() {
            let card = self.storage.get_contact_card(contact_id)?;
            if card.trust_state != TrustState::Verified {
                return Err(OmniError::InvalidInput(
                    "remote access can be granted only to a verified contact; verify the contact first".into(),
                ));
            }
        }
        let record = self.storage.set_contact_remote_access_granted(contact_id, mode, current_unix_ms())?;
        if mode.is_none() {
            // Revoked mid-session: the session ends now, not when the controller lets go.
            self.end_remote_control_owned_by(contact_id);
        }
        // Best-effort: an offline contact hears it on its next link (the on-link re-send).
        let _ = self.send_remote_access_grant_to_contact(contact_id);
        Ok(record)
    }

    /// Start controlling this contact's computer: a call of its own kind (no microphone, no camera)
    /// whose invite asks for control. Refused here if the contact never told us it allows it; even
    /// when it did, the contact's core decides again (its grant may be gone) and may ask its owner.
    pub fn request_remote_control(&self, contact_id: &ContactId) -> OmniResult<omni_types::CallSummary> {
        let _ = self.ensure_runtime_profile_active("request_remote_control")?;
        if self.storage.get_contact_remote_access(contact_id)?.received_mode.is_none() {
            return Err(OmniError::InvalidInput(
                "this contact has not allowed you to control their computer".into(),
            ));
        }
        let contact = self.storage.get_contact(contact_id)?;
        let dialog_key = contact
            .primary_dialog_key
            .ok_or_else(|| OmniError::InvalidInput("the contact has no dialog yet".into()))?;
        self.place_call_of_kind(&dialog_key, true)
    }

    fn end_remote_control_owned_by(&self, contact_id: &ContactId) {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let Ok(Some(runtime)) = crate::current_call_runtime(&db_path) else {
            return;
        };
        let owned_by_contact = runtime.summary.contact_id.as_deref() == Some(contact_id.0.as_str())
            && runtime
                .summary
                .remote_control
                .is_some_and(|session| session.role == omni_types::RemoteControlRole::Owner);
        if owned_by_contact {
            let call_id = runtime.summary.call_id.clone();
            let _ = match runtime.summary.state {
                omni_types::CallState::Ringing => {
                    self.decline_call(&call_id, omni_types::CallDeclineReason::SecurityBlocked)
                }
                _ => self.hang_up_call(&call_id),
            };
        }
    }

    fn send_remote_access_grant_to_contact(&self, contact_id: &ContactId) -> OmniResult<()> {
        let contact = self.storage.get_contact(contact_id)?;
        let (Some(remote_account_id), Some(dialog_key)) =
            (contact.remote_account_id.as_ref(), contact.primary_dialog_key.as_deref())
        else {
            return Ok(());
        };
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let Some(session) = self.storage.get_session_for_dialog(remote_account_id, dialog_key)? else {
            return Ok(());
        };
        crate::send_remote_access_grant_for_session(&self.storage, &self.transport, &session, dialog_key)
    }
}

/// A contact told us whether it lets us control its computer (inbound `RemoteAccessGrantV1`). Stored
/// so we can show the "Управлять" affordance; the contact's own core still decides each session.
pub(crate) fn handle_incoming_remote_access_grant(
    storage: &omni_storage::Storage,
    dialog_key: &str,
    grant: &omni_types::RemoteAccessGrantV1,
) -> OmniResult<()> {
    let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? else {
        return Ok(()); // a grant from someone who is not a contact we track changes nothing
    };
    storage.set_contact_remote_access_received(&contact.contact_id, grant.mode, grant.sent_at_unix_ms)?;
    Ok(())
}
