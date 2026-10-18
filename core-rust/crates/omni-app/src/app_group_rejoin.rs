// A device restored from the phrase comes back into the account's groups
// (docs/decisions/group-history-sync.md, (2); account-recovery-from-friends.md).
//
// The phrase restores the account, not its MLS leaves: the new device is a new member of every group
// the account was in (the passport lists them). It asks a contact, over their 1:1 session, to add it
// — a `device_join` carrying a fresh KeyPackage, like accepting an invitation nobody sent. A contact
// who is in the group adds it only if the KeyPackage is of the device on the other end of that
// session (its key certified by the account, as every prekey bundle is) and a device of the same
// account is already a member: the account was let in once, by consent; a new device of it is the
// same person. Anyone else's request is ignored without a word. The Welcome and the recent history
// come back as for any newcomer.
//
// One contact at a time, the next one ten minutes later (two members adding the same device at once
// would split the group); a contact not in the group just ignores it. It stops once we are in, or a
// month after the restore.
//
// A second device of the account that is alive (stage 4 of docs/decisions/multi-device-equal-nodes.md)
// comes in the same way, from the other side: a member device of ours sees our other device is not
// in a group and tells it (`device_offer`); that device asks it (`device_join`) and is added. One ask
// per group in ten minutes, however many of our devices offer.

use base64::Engine as _;

use omni_protocol::{GroupInviteAcceptV2, GROUP_WIRE_VERSION_V2};
use omni_storage::{GroupInviteRecord, Storage};
use omni_identity::{AccountId, ContactRelationshipState};
use omni_types::OmniResult;

use crate::app_groups::{own_key_package, with_engine};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The next contact is asked this long after the previous one.
const DEVICE_JOIN_ASK_EVERY_MS: i64 = 10 * 60 * 1000;
/// A restored device stops asking this long after the restore.
const DEVICE_JOIN_GIVE_UP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

fn ask_state_key(group_id: &str) -> String {
    format!("group_device_join:{group_id}")
}

/// The `member_pub`s (device signing keys, base64) of `account_id`'s devices we hold certified:
/// a contact's from their prekey bundles, ours from the account's device list. Revoked ones are out.
pub(crate) fn account_device_pubs(storage: &Storage, account_id: &AccountId) -> OmniResult<Vec<String>> {
    let own = storage.get_local_account()?.is_some_and(|a| &a.descriptor.account_id == account_id);
    let mut pubs: Vec<String> = if own {
        storage
            .list_account_devices(account_id)?
            .into_iter()
            .filter(|device| device.revoked_at_unix_ms.is_none())
            .map(|device| B64.encode(device.chain.device.identity_signing_public_key.0))
            .collect()
    } else {
        storage
            .list_remote_prekey_bundles_for_account(account_id)?
            .into_iter()
            .filter(|bundle| bundle.device_certificate_chain.is_some())
            .map(|bundle| B64.encode(bundle.identity_signing_key.0))
            .collect()
    };
    pubs.sort();
    pubs.dedup();
    Ok(pubs)
}

/// Our side: what to send a contact to be added to `group_id` — and the invitation, marked accepted,
/// that lets the Welcome in when it comes.
pub(crate) fn device_join_request(
    storage: &Storage,
    group_id: &str,
    name: &str,
    contact_dialog_key: &str,
    contact_name: &str,
    now: i64,
) -> OmniResult<GroupInviteAcceptV2> {
    storage.upsert_group_invite(&GroupInviteRecord {
        group_id: group_id.to_string(),
        name: name.to_string(),
        inviter_name: contact_name.to_string(),
        inviter_dialog_key: contact_dialog_key.to_string(),
        status: "accepted".into(),
        received_at_unix_ms: now,
    })?;
    Ok(GroupInviteAcceptV2 {
        version: GROUP_WIRE_VERSION_V2,
        group_id: group_id.to_string(),
        key_package_b64: B64.encode(own_key_package(storage)?),
    })
}

/// A member's side: `sender_account`'s device on `sender_dialog` asks to come into the group. Added
/// only if the KeyPackage is of one of the account's certified devices (`known_pubs`), that device
/// is not in yet, and another device of the account is. `None` — not for us to do; nothing is said.
pub(crate) fn add_account_device(
    storage: &Storage,
    sender_dialog: &str,
    sender_account: &AccountId,
    known_pubs: &[String],
    request: &GroupInviteAcceptV2,
) -> OmniResult<Option<omni_protocol::GroupWelcomeV2>> {
    omni_protocol::validate_group_invite_accept_v2(request)?;
    if storage.get_group(&request.group_id)?.is_none_or(|g| g.my_status != "member") {
        return Ok(None);
    }
    let gid = crate::app_groups::group_id_bytes(&request.group_id)?;
    let Ok(key_package) = B64.decode(&request.key_package_b64) else {
        return Ok(None);
    };
    let Ok(card) = with_engine(storage, |engine| engine.key_package_card(&key_package)) else {
        return Ok(None);
    };
    if card.account_id != sender_account.0 || !known_pubs.contains(&card.member_pub) {
        return Ok(None);
    }
    let view = with_engine(storage, |engine| engine.view(&gid))?;
    if !view.active || view.members.iter().any(|m| m.card.member_pub == card.member_pub) {
        return Ok(None);
    }
    let account_is_in = view
        .members
        .iter()
        .any(|m| m.card.account_id == sender_account.0 && known_pubs.contains(&m.card.member_pub));
    if !account_is_in {
        return Ok(None);
    }
    storage.record_group_sent_invite(&request.group_id, sender_dialog, crate::current_unix_ms())?;
    crate::app_groups::add_invitee(storage, sender_dialog, request)
}

/// We asked to be added to `group_id` less than ten minutes ago (an ask still on its way).
fn asked_recently(storage: &Storage, group_id: &str, now: i64) -> bool {
    storage
        .get_group_invite(group_id)
        .ok()
        .flatten()
        .is_some_and(|invite| invite.status == "accepted" && now - invite.received_at_unix_ms < DEVICE_JOIN_ASK_EVERY_MS)
}

/// Our own device says we are not in `offer`'s group: ask it to add us, unless we are in or asked.
pub(crate) fn answer_device_offer(
    storage: &Storage,
    transport: &crate::TransportManager,
    dialog_key: &str,
    offer: &omni_protocol::GroupInviteV2,
) -> OmniResult<()> {
    let now = crate::current_unix_ms();
    if storage.get_group(&offer.group_id)?.is_some_and(|g| g.my_status == "member") || asked_recently(storage, &offer.group_id, now) {
        return Ok(());
    }
    let request = device_join_request(storage, &offer.group_id, &offer.name, dialog_key, "ваше устройство", now)?;
    crate::app_groups::send_device_join(storage, transport, dialog_key, &request)
}

/// Offer each group we are in to each of our other devices that is not in it (it asks back).
pub(crate) fn offer_groups_to_own_devices(storage: &Storage, transport: &crate::TransportManager, now: i64) {
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account = account.descriptor.account_id;
    let Ok(devices) = storage.list_account_devices(&own_account) else {
        return;
    };
    let Ok(dialogs) = storage.list_own_device_dialogs() else {
        return;
    };
    let Ok(groups) = storage.list_groups() else {
        return;
    };
    for group in groups.into_iter().filter(|g| g.my_status == "member") {
        let Ok(members) = storage.list_group_members(&group.group_id) else {
            continue;
        };
        for (device_id, dialog_key) in &dialogs {
            let Some(device) = devices.iter().find(|d| &d.chain.device.device_id == device_id && d.revoked_at_unix_ms.is_none()) else {
                continue;
            };
            let device_pub = B64.encode(device.chain.device.identity_signing_public_key.0);
            if members.iter().any(|m| m.member_pub == device_pub && m.status == "member") {
                continue;
            }
            let key = format!("group_device_offer:{}:{}", group.group_id, device_id.0);
            let offered_at = storage.get_app_meta(&key).ok().flatten().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            if now - offered_at < DEVICE_JOIN_ASK_EVERY_MS {
                continue;
            }
            if transport.connection_snapshot_for_dialog(dialog_key).is_none() {
                continue;
            }
            let offer = omni_protocol::GroupInviteV2 {
                version: GROUP_WIRE_VERSION_V2,
                group_id: group.group_id.clone(),
                name: group.name.clone(),
                inviter_name: account.descriptor.display_name.clone(),
                renew: false,
                authored_at_unix_ms: now,
            };
            let _ = storage.upsert_app_meta(&key, &now.to_string());
            if let Err(error) = crate::app_groups::send_device_offer(storage, transport, dialog_key, &offer) {
                crate::omni_app_trace_log("group/device-offer", &format!("group={} device={} not told: {error}", group.group_id, device_id.0));
            }
        }
    }
}

/// Ask the next reachable contact to add us to each group of the account we are not in yet.
pub(crate) fn group_rejoin_round(storage: &Storage, transport: &crate::TransportManager, now: i64) {
    let Ok(groups) = storage.list_recovered_groups() else {
        return;
    };
    for group in groups {
        if now - group.recovered_at_unix_ms > DEVICE_JOIN_GIVE_UP_MS {
            continue;
        }
        if storage.get_group(&group.group_id).ok().flatten().is_some_and(|g| g.my_status == "member")
            || asked_recently(storage, &group.group_id, now)
        {
            continue;
        }
        let state = storage.get_app_meta(&ask_state_key(&group.group_id)).ok().flatten().unwrap_or_default();
        let (asked_at, next) = state
            .split_once(':')
            .map(|(at, next)| (at.parse::<i64>().unwrap_or(0), next.parse::<usize>().unwrap_or(0)))
            .unwrap_or((0, 0));
        if now - asked_at < DEVICE_JOIN_ASK_EVERY_MS {
            continue;
        }
        let contacts: Vec<_> = storage
            .list_contacts()
            .unwrap_or_default()
            .into_iter()
            .filter(|contact| {
                contact.remote_account_id.is_some() && contact.relationship_state == ContactRelationshipState::Approved
            })
            .collect();
        if contacts.is_empty() {
            continue;
        }
        for offset in 0..contacts.len() {
            let index = (next + offset) % contacts.len();
            let contact = &contacts[index];
            let Some(account_id) = contact.remote_account_id.as_ref() else {
                continue;
            };
            let Some(dialog_key) = crate::reachable_session_dialog(storage, transport, account_id) else {
                continue;
            };
            let contact_name = if contact.petname.trim().is_empty() {
                contact.public_display_name.clone().unwrap_or_default()
            } else {
                contact.petname.clone()
            };
            let sent = device_join_request(storage, &group.group_id, &group.name, &dialog_key, &contact_name, now)
                .and_then(|request| crate::app_groups::send_device_join(storage, transport, &dialog_key, &request));
            if let Err(error) = &sent {
                crate::omni_app_trace_log("group/rejoin", &format!("group={} not asked: {error}", group.group_id));
            }
            let _ = storage.upsert_app_meta(&ask_state_key(&group.group_id), &format!("{now}:{}", index + 1));
            break;
        }
    }
}
