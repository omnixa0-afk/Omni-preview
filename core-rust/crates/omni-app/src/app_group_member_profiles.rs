// A group member who is not our contact: its name and photo, from the member itself
// (docs/decisions/group-member-profile.md). Owner decision 2026-10-13: the photo is seen by the members
// of the groups we share, as in other messengers.
//
// Each member sends its card into the group (`Profile`) when it joins and whenever it changes its
// profile; a newcomer asks everyone once (`ProfileAsk`) and the members answer — not more often than
// once a minute per group, however many newcomers ask. Kept one per account, whatever groups we share,
// and forgotten when the account is in none of them. A contact's own profile (over 1:1) always wins in
// what is shown; the card is for the people we do not have.

use base64::Engine as _;

use omni_group_mls::MemberCard;
use omni_protocol::{
    validate_group_member_profile_v1, GroupAppKindV1, GroupAppPayloadV1, GroupMemberProfileV1,
    GROUP_MEMBER_AVATAR_MAX_BYTES,
};
use omni_storage::Storage;
use omni_types::{OmniError, OmniResult};

use crate::app_groups::{random_group_message_id, send_app_payload};
use crate::current_unix_ms;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A group's members answer a newcomer's ask at most this often.
const PROFILE_ANSWER_EVERY_MS: i64 = 60 * 1000;

fn answered_key(group_id: &str) -> String {
    format!("group_profile_answered:{group_id}")
}

/// Our card as of now: the profile's name and a small copy of its photo.
pub(crate) fn own_member_profile(storage: &Storage) -> OmniResult<GroupMemberProfileV1> {
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::InvalidInput("no local account".into()))?;
    let profile = storage.get_local_public_profile_state()?;
    let avatar = profile
        .avatar_bytes
        .as_deref()
        .map(|bytes| omni_storage::shrink_avatar_for_group_card(bytes, GROUP_MEMBER_AVATAR_MAX_BYTES))
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| B64.encode(bytes));
    Ok(GroupMemberProfileV1 {
        account_id: account.descriptor.account_id.0,
        version: profile.updated_at_unix_ms,
        display_name: profile.display_name,
        avatar_jpeg_b64: avatar,
    })
}

fn say(storage: &Storage, group_id: &str, kind: GroupAppKindV1, body: String) -> OmniResult<()> {
    send_app_payload(
        storage,
        group_id,
        &GroupAppPayloadV1 {
            message_id: random_group_message_id(),
            kind,
            body,
            authored_at_unix_ms: current_unix_ms(),
            signature_b64: None,
        },
    )
}

/// Our card into one group.
pub(crate) fn send_own_profile(storage: &Storage, group_id: &str) -> OmniResult<()> {
    let card = own_member_profile(storage)?;
    let body = serde_json::to_string(&card).map_err(|error| OmniError::Internal(error.to_string()))?;
    say(storage, group_id, GroupAppKindV1::Profile, body)
}

/// We just joined: our card to everyone, and theirs asked for.
pub(crate) fn introduce_after_join(storage: &Storage, group_id: &str) {
    if let Err(error) = send_own_profile(storage, group_id).and_then(|_| say(storage, group_id, GroupAppKindV1::ProfileAsk, String::new())) {
        crate::omni_app_trace_log("group/profile", &format!("group={group_id} introduce failed: {error}"));
    }
}

/// Our profile changed: the new card into every group we are in.
pub(crate) fn broadcast_own_profile(storage: &Storage) {
    let Ok(groups) = storage.list_groups() else {
        return;
    };
    for group in groups.iter().filter(|group| group.my_status == "member") {
        if let Err(error) = send_own_profile(storage, &group.group_id) {
            crate::omni_app_trace_log("group/profile", &format!("group={} send failed: {error}", group.group_id));
        }
    }
}

/// A newcomer asked: our card, unless we answered in this group a moment ago.
pub(crate) fn answer_profile_ask(storage: &Storage, group_id: &str) -> OmniResult<()> {
    let now = current_unix_ms();
    let last = storage
        .get_app_meta(&answered_key(group_id))?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < PROFILE_ANSWER_EVERY_MS {
        return Ok(());
    }
    storage.upsert_app_meta(&answered_key(group_id), &now.to_string())?;
    send_own_profile(storage, group_id)
}

/// A member's card arrived. Taken only for the sender's own account; returns whether it changed
/// what we show.
pub(crate) fn receive_profile(storage: &Storage, sender: &MemberCard, body: &str) -> OmniResult<bool> {
    let Ok(card) = serde_json::from_str::<GroupMemberProfileV1>(body) else {
        return Ok(false);
    };
    validate_group_member_profile_v1(&card)?;
    if card.account_id != sender.account_id {
        crate::omni_app_trace_log("group/profile", "a card for another account ignored");
        return Ok(false);
    }
    let avatar = match card.avatar_jpeg_b64.as_deref() {
        // A photo that does not decode is dropped, the name is still taken.
        Some(b64) => B64
            .decode(b64)
            .ok()
            .filter(|bytes| bytes.len() <= GROUP_MEMBER_AVATAR_MAX_BYTES && bytes.starts_with(&[0xFF, 0xD8])),
        None => None,
    };
    storage.store_group_member_profile(&card.account_id, card.version, card.display_name.trim(), avatar.as_deref(), current_unix_ms())
}
