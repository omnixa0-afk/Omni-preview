// Group orchestration on MLS — docs/decisions/group-crypto-mls.md.
//
// `omni-group-mls` is the group's crypto and membership (OpenMLS + our Commit ordering); this file
// wires it to the rest of the app:
//
// - Consent over the pairwise 1:1 channel: Invite → (user accepts) Accept carrying a fresh MLS
//   KeyPackage → the inviter adds them (any member may) and sends the MLS Welcome back.
// - Everything else on the group's durable feed (subscription + gossip + anti-entropy): Commits and
//   MLS application messages (text, call control, "leaving"). Publishing goes through ONE local
//   queue (`group_feed_outbox`) flushed by whoever holds the mesh — so a path without a mesh handle
//   (the 1:1 receive) and a moment without a mesh both just queue.
// - A projection of the members (`group_members`), refreshed from MLS after every Commit, for the UI
//   and calls; who wrote each message (`group_message_meta`); system lines for joins and departures.
// - A Commit of ours that loses its epoch to a concurrent one is re-issued; our messages from a
//   rolled-back epoch are re-sent; messages for an epoch not reached yet wait and are retried.
//
// MLS state at rest is encrypted (`ProtectedCodec`, AES-256-GCM-SIV under a platform-protected key).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;

use omni_group_mls::{CommitOutcome, Engine, GroupError, GroupView, Inbound, MemberCard};
use omni_protocol::{
    GroupAppKindV1, GroupAppPayloadV1, GroupCallControlV1, GroupFeedItemV2, GroupInviteAcceptV2,
    GroupInviteDeclineV2, GroupInviteV2, GroupWelcomeV2, GROUP_WIRE_VERSION_V2,
};
use omni_storage::{
    group_dialog_key, GroupInviteRecord, GroupMemberRecord, GroupMessageMeta, GroupRecord, Storage,
};
use omni_types::{OmniError, OmniResult};

use crate::{current_unix_ms, OmniApp};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

// --- shell-facing views ------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupSummary {
    pub group_id: String,
    pub name: String,
    pub dialog_key: String,
    /// admin | member
    pub my_role: String,
    /// member | removed (by an admin) | dissolved (an admin deleted the group); a left group is gone
    pub my_status: String,
    pub member_count: usize,
    pub active_member_count: usize,
    pub epoch: u64,
    /// "Автор: текст" of the latest message, for the chat list.
    #[serde(default)]
    pub last_message_preview: String,
    #[serde(default)]
    pub last_activity_unix_ms: i64,
    /// Members' lines this device has not read (system lines do not count).
    #[serde(default)]
    pub unread_count: u32,
    /// The group's picture file, if it has one.
    #[serde(default)]
    pub avatar_path: Option<String>,
    /// Newcomers get the recent history (an admin's switch, on by default).
    #[serde(default = "newcomer_history_default")]
    pub newcomer_history: bool,
    /// The group has git repositories (docs/decisions/omni-workspace-git.md): the chat list marks it.
    #[serde(default)]
    pub is_workspace: bool,
}

fn newcomer_history_default() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupMemberView {
    pub member_pub: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub is_me: bool,
    /// The member's account — lets a contact picker say "already in the group".
    #[serde(default)]
    pub account_id: String,
    /// The member's photo: the contact's own when it is a contact, else the one from its group card
    /// (docs/decisions/group-member-profile.md); none — the shell draws a letter.
    #[serde(default)]
    pub avatar_path: Option<String>,
}

/// An invitation WE sent for a group that is still unanswered. Only the inviter knows about it:
/// the invitation travels over 1:1, the rest of the group learns of the person when they join.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupSentInviteView {
    pub dialog_key: String,
    pub display_name: String,
    pub sent_at_unix_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupInviteView {
    pub group_id: String,
    pub name: String,
    pub inviter_name: String,
    pub inviter_dialog_key: String,
    pub received_at_unix_ms: i64,
    /// pending (unanswered) | accepted (joining — waiting for the Welcome)
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupMessageView {
    pub message_id: String,
    /// None for a system line.
    pub sender_pub: Option<String>,
    pub sender_name: String,
    pub is_mine: bool,
    /// text | attachment | system
    pub kind: String,
    pub body: String,
    pub status: String,
    pub created_at_unix_ms: i64,
    /// The file or photo of an `attachment` line, with its transfer state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<omni_types::AttachmentRecord>,
    /// The line this one answers, as its quote shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<omni_types::MessageReplyPreview>,
    /// The emoji on it; `who` holds names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<omni_types::MessageReactionView>,
    /// It is pinned in the group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

// --- MLS state at rest -------------------------------------------------------------------------

thread_local! {
    /// The MLS storage key of the database the engine on THIS thread is working on. The OpenMLS codec
    /// is a type with static functions (no instance to carry a key), and every engine call runs
    /// synchronously inside `with_engine` on one thread, so the key is scoped there.
    static MLS_STORAGE_KEY: Cell<Option<[u8; 32]>> = const { Cell::new(None) };
}

#[derive(Debug)]
pub(crate) enum CodecError {
    Json(serde_json::Error),
    NoKey,
    Crypto,
    Format,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Json(e) => write!(f, "mls codec json: {e}"),
            CodecError::NoKey => write!(f, "mls codec: no storage key in scope"),
            CodecError::Crypto => write!(f, "mls codec: decrypt failed"),
            CodecError::Format => write!(f, "mls codec: unknown format"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Deterministic authenticated encryption of every OpenMLS stored value AND key (the SQLite provider
/// looks rows up by the encoded key, so encryption must be deterministic: AES-256-GCM-SIV with a fixed
/// nonce — it leaks only equality, which lookups need anyway). The key is per database, wrapped by
/// the platform (`protect_bytes`: DPAPI on Windows, the host-provided key on Android).
#[derive(Default)]
pub(crate) struct ProtectedCodec;

const CODEC_TAG: &[u8; 2] = b"o1";

impl omni_group_mls::Codec for ProtectedCodec {
    type Error = CodecError;

    fn to_vec<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Self::Error> {
        use aes_gcm_siv::aead::{Aead, KeyInit};
        let key = MLS_STORAGE_KEY.with(Cell::get).ok_or(CodecError::NoKey)?;
        let plain = serde_json::to_vec(value).map_err(CodecError::Json)?;
        let cipher = aes_gcm_siv::Aes256GcmSiv::new((&key).into());
        let sealed = cipher
            .encrypt(&aes_gcm_siv::Nonce::default(), plain.as_slice())
            .map_err(|_| CodecError::Crypto)?;
        let mut out = CODEC_TAG.to_vec();
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn from_slice<T: serde::de::DeserializeOwned>(slice: &[u8]) -> Result<T, Self::Error> {
        use aes_gcm_siv::aead::{Aead, KeyInit};
        let sealed = slice.strip_prefix(CODEC_TAG.as_slice()).ok_or(CodecError::Format)?;
        let key = MLS_STORAGE_KEY.with(Cell::get).ok_or(CodecError::NoKey)?;
        let cipher = aes_gcm_siv::Aes256GcmSiv::new((&key).into());
        let plain = cipher
            .decrypt(&aes_gcm_siv::Nonce::default(), sealed)
            .map_err(|_| CodecError::Crypto)?;
        serde_json::from_slice(&plain).map_err(CodecError::Json)
    }
}

/// The key the active profile's MLS state is sealed with — one per profile: with one per database
/// two profiles in one group shared their OpenMLS rows, and a profile's copy carried the others' MLS
/// secrets (blind audit 2026-11-12, pass 17). As the codec is deterministic, the key also keeps the
/// rows of profiles apart: the same group id seals to another lookup key. The unsealed key is cached
/// with the very record it came from, and the record is read each time: the key is the database's,
/// not the process's. Cached by path alone, it outlived the record — the last profile's deletion
/// wipes the MLS state with its key, a backup restored over the database brings another one — and
/// new groups were sealed with a key no longer stored: unreadable after a restart (blind audit
/// 2026-11-10).
fn mls_storage_key(storage: &Storage) -> OmniResult<[u8; 32]> {
    if storage.get_mls_storage_key_protected()?.is_none() {
        let fresh = omni_crypto::generate_group_key();
        let protected = omni_crypto::protect_bytes(&fresh)?;
        let json = serde_json::to_string(&protected).map_err(|e| OmniError::Internal(e.to_string()))?;
        storage.set_mls_storage_key_protected(&json)?;
    }
    // Re-read: a racing first writer's key wins (INSERT OR IGNORE).
    let json = storage
        .get_mls_storage_key_protected()?
        .ok_or_else(|| OmniError::Internal("mls storage key vanished".into()))?;
    unseal_mls_storage_key(storage, &json)
}

fn unseal_mls_storage_key(storage: &Storage, json: &str) -> OmniResult<[u8; 32]> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, [u8; 32])>>> = OnceLock::new();
    let db = storage.db_path().to_string_lossy().to_string();
    let cached = CACHE
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|cache| cache.get(&db).filter(|(record, _)| record == json).map(|(_, key)| *key));
    if let Some(key) = cached {
        return Ok(key);
    }
    let protected: omni_crypto::ProtectedSecret =
        serde_json::from_str(json).map_err(|e| OmniError::Internal(format!("mls key: {e}")))?;
    let raw = omni_crypto::unprotect_bytes(&protected)?;
    let key: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| OmniError::Internal("mls storage key has a wrong size".into()))?;
    if let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() {
        cache.insert(db, (json.to_string(), key));
    }
    Ok(key)
}

/// Whether one of a row's blob values was sealed by the codec with `key`: the row is that profile's.
fn sealed_with(key: &[u8; 32], blobs: &[Vec<u8>]) -> bool {
    use aes_gcm_siv::aead::{Aead, KeyInit};
    let cipher = aes_gcm_siv::Aes256GcmSiv::new(key.into());
    blobs.iter().any(|blob| {
        blob.strip_prefix(CODEC_TAG.as_slice())
            .is_some_and(|sealed| cipher.decrypt(&aes_gcm_siv::Nonce::default(), sealed).is_ok())
    })
}

/// A profile's deletion: its MLS rows go, by the key they are sealed with, active profile or not —
/// the rows were not profile-scoped and stayed on disk (blind audit 2026-11-12, pass 17). The key
/// itself goes with the profile's rows.
pub(crate) fn forget_profile_mls(storage: &Storage, profile_id: &omni_identity::LocalProfileId) -> OmniResult<usize> {
    let Some(json) = storage.mls_storage_key_protected_of(profile_id)? else {
        return Ok(0);
    };
    // The database's old key, still held by another profile here: its rows are that one's too.
    if storage.mls_storage_key_shared_with_another_profile(profile_id)? {
        return Ok(0);
    }
    let key = unseal_mls_storage_key(storage, &json)?;
    storage.retain_mls_rows(&|blobs| !sealed_with(&key, blobs))
}

/// A copy made for the active profile keeps its MLS rows alone: the others' are sealed with their
/// own keys, which leave the copy with their profiles, but the rows themselves were theirs to keep.
pub(crate) fn keep_own_mls_in_copy(storage: &Storage, copy: &Storage) -> OmniResult<usize> {
    let key = mls_storage_key(storage)?;
    copy.retain_mls_rows(&|blobs| sealed_with(&key, blobs))
}

/// Restores the previous thread-local key when the engine scope ends.
struct KeyScope(Option<[u8; 32]>);

impl Drop for KeyScope {
    fn drop(&mut self) {
        MLS_STORAGE_KEY.with(|cell| cell.set(self.0));
    }
}

fn map_group_error(error: GroupError) -> OmniError {
    match error {
        GroupError::NotAllowed(what) => OmniError::InvalidInput(what.to_string()),
        GroupError::Invalid(what) => OmniError::InvalidInput(what),
        GroupError::UnknownGroup => OmniError::InvalidInput("unknown group".into()),
        other => OmniError::Internal(format!("group: {other}")),
    }
}

/// Our MLS identity: the account Ed25519 key (it signs MLS) and the card the credential carries.
pub(crate) fn local_mls_identity(storage: &Storage) -> OmniResult<([u8; 32], MemberCard)> {
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::Internal("no local account".into()))?;
    let identity = storage
        .get_local_identity_key(&account.descriptor.account_id)?
        .ok_or_else(|| OmniError::Internal("no identity key".into()))?;
    let seed = omni_crypto::mesh_identity_signing_key_bytes(&identity)?;
    // The device chain makes the account in our card a fact the members check, not a claim.
    let device_chain = storage
        .get_local_device_certificate_chain()?
        .map(|chain| B64.encode(chain.to_compact()))
        .unwrap_or_default();
    Ok((
        seed,
        MemberCard {
            member_pub: B64.encode(identity.signing_public_key.0),
            account_id: account.descriptor.account_id.0.clone(),
            display_name: account.descriptor.display_name.clone(),
            device_chain,
        },
    ))
}

/// The device a member card names, as its chain says (not yet verified — see [`check_member_card`]).
pub(crate) fn card_device_chain(card: &MemberCard) -> Option<omni_crypto::DeviceCertificateChainV1> {
    let bytes = B64.decode(&card.device_chain).ok()?;
    omni_crypto::DeviceCertificateChainV1::from_compact(&bytes)
}

/// The MLS engine's check of a member card (`omni_group_mls::CardCheck`): its device chain holds, the
/// chain's device key is the key that signs the leaf, and the card's account is the chain's. Before
/// it, a member's KeyPackage could name any account — an admin's, to pass for them where the app
/// trusts the account (a workspace review), or a server's own, to be served what only its own devices
/// get (audit 2026-11-09, F01).
fn check_member_card(card: &MemberCard, signature_key: &[u8]) -> Result<(), String> {
    let chain = card_device_chain(card).ok_or_else(|| "the member card's device chain is unreadable".to_string())?;
    omni_crypto::verify_device_certificate_chain(&chain).map_err(|error| format!("the member card's device chain: {error}"))?;
    if chain.device.identity_signing_public_key.0.as_slice() != signature_key {
        return Err("the member card's device chain is for another key".into());
    }
    if chain.account_id().0 != card.account_id {
        return Err("the member card names another account than its device chain".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn mls_storage_key_for_tests(storage: &Storage) -> OmniResult<[u8; 32]> {
    mls_storage_key(storage)
}

/// Tests: how many MLS rows of the database are sealed with `key`.
#[cfg(test)]
pub(crate) fn mls_rows_sealed_with_for_tests(storage: &Storage, key: &[u8; 32]) -> usize {
    let count = std::cell::Cell::new(0);
    storage
        .retain_mls_rows(&|blobs| {
            if sealed_with(key, blobs) {
                count.set(count.get() + 1);
            }
            true
        })
        .unwrap();
    count.get()
}

#[cfg(test)]
pub(crate) fn check_member_card_for_tests(card: &MemberCard, signature_key: &[u8]) -> Result<(), String> {
    check_member_card(card, signature_key)
}

/// The card with its account kept only when its device chain proves it, else empty. A card from
/// before cards carried chains named its account unchecked (audit 2026-11-09, F01); its member puts
/// the chain in on its next sweep (`refresh_own_cards_in_groups`).
pub(crate) fn card_with_proven_account(card: &MemberCard) -> MemberCard {
    let proven = !card.device_chain.is_empty()
        && B64.decode(&card.member_pub).is_ok_and(|key| check_member_card(card, &key).is_ok());
    if proven {
        card.clone()
    } else {
        MemberCard { account_id: String::new(), ..card.clone() }
    }
}

/// Put our card with its device chain into every group whose leaf of ours still carries one without
/// (joined before cards carried chains): until then the members do not take our account as proven.
/// Returns how many groups changed.
pub(crate) fn refresh_own_cards_in_groups(storage: &Storage) -> usize {
    let Ok(groups) = storage.list_groups() else {
        return 0;
    };
    let mut changed = 0;
    for group in groups.into_iter().filter(|g| g.my_status == "member") {
        let Ok(gid) = group_id_bytes(&group.group_id) else { continue };
        let refreshed = in_mls_transaction(storage, "group_own_commit", |storage| {
            let commit = with_engine(storage, |engine| engine.refresh_own_card(&gid))?;
            if let Some(commit) = &commit {
                own_commit_bookkeeping(storage, &group.group_id, commit, &OwnChange::RefreshCard)?;
            }
            Ok(commit)
        });
        match refreshed {
            Ok(Some(_)) => {
                changed += 1;
                crate::omni_app_trace_log("group/card", &format!("group={} our card now carries its device chain", group.group_id));
            }
            Ok(None) => {}
            Err(error) => crate::omni_app_trace_log("group/card", &format!("group={} card not refreshed: {error}", group.group_id)),
        }
    }
    changed
}

/// Whether a member card's device is one its account revoked, as far as we know (our own account's
/// revocations always; another account's when it told us).
pub(crate) fn card_device_revoked(storage: &Storage, card: &MemberCard) -> bool {
    card_device_chain(card)
        .is_some_and(|chain| storage.is_device_revoked(&chain.account_id(), &chain.device.device_id).unwrap_or(false))
}

/// Take revoked devices out of our groups: a revocation does not reach a group by itself, and a
/// revoked device that ignores its order to wipe kept the group's keys (audit 2026-11-09, F02). We may
/// remove a device of our own account in any group, any device as an admin; the members check that
/// rule on the Commit. As an admin, also the members that said they left and are still in: their
/// farewell's Commit failed or lost a race, and they held the epoch's keys (code audit 2026-11-09,
/// OMNI-02). Returns how many groups changed.
pub(crate) fn remove_revoked_and_departed_from_groups(storage: &Storage) -> usize {
    let Ok(groups) = storage.list_groups() else {
        return 0;
    };
    let mut changed = 0;
    for group in groups.into_iter().filter(|g| g.my_status == "member") {
        let Ok(gid) = group_id_bytes(&group.group_id) else { continue };
        let Ok(view) = with_engine(storage, |engine| engine.view(&gid)) else { continue };
        let we_are_admin = view.members.iter().any(|m| m.is_me && m.is_admin);
        let left: std::collections::HashSet<String> = if we_are_admin {
            storage
                .list_group_members(&group.group_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.status == "left")
                .map(|m| m.member_pub)
                .collect()
        } else {
            Default::default()
        };
        let pubs: Vec<String> = view
            .members
            .iter()
            .filter(|m| !m.is_me && (card_device_revoked(storage, &m.card) || left.contains(&m.card.member_pub)))
            .map(|m| m.card.member_pub.clone())
            .collect();
        if pubs.is_empty() {
            continue;
        }
        let removed = in_mls_transaction(storage, "group_own_commit", |storage| {
            let commit = with_engine(storage, |engine| engine.remove_members(&gid, &pubs))?;
            own_commit_bookkeeping(storage, &group.group_id, &commit, &OwnChange::Remove { member_pubs: pubs.clone() })
        });
        match removed {
            Ok(()) => {
                changed += 1;
                crate::omni_app_trace_log("group/revoked", &format!("group={} removed {} revoked or departed device(s)", group.group_id, pubs.len()));
            }
            Err(error) => crate::omni_app_trace_log(
                "group/revoked",
                &format!("group={} {} revoked or departed device(s) not ours to remove: {error}", group.group_id, pubs.len()),
            ),
        }
    }
    changed
}

/// Run `f` with this profile's MLS engine, on the app's own connection and as one transaction (or
/// inside the caller's): the engine had a connection of its own, so its Commit and the app's rows
/// around it (the feed, the own change, the view) could not be one step — a failure between them
/// left our MLS state an epoch ahead of a Commit nobody got (blind audit 2026-11-12, pass 17).
pub(crate) fn with_engine<T>(
    storage: &Storage,
    f: impl FnOnce(&Engine<'_, ProtectedCodec>) -> omni_group_mls::Result<T>,
) -> OmniResult<T> {
    let key = mls_storage_key(storage)?;
    let _scope = KeyScope(MLS_STORAGE_KEY.with(|cell| cell.replace(Some(key))));
    let (seed, me) = local_mls_identity(storage)?;
    in_mls_transaction(storage, "mls_step", |storage| {
        let engine = Engine::<ProtectedCodec>::new(storage.connection_for_mls_engine(), seed, me).with_card_check(check_member_card);
        f(&engine).map_err(map_group_error)
    })
}

/// One MLS step at a time: each loads the group, changes it and stores it, so two at once (a files
/// project's thread sending beside the feed being taken in) stored one over the other and our next
/// message reused a generation the members had used up — they dropped it and every later one until
/// the counter caught up (live 2026-10-31). Reentrant: a step may call another; one lock for every
/// profile, as only one is active.
static ENGINE: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());

/// `f` as one transaction that MLS steps are part of. The engine's lock is taken BEFORE the
/// transaction begins, everywhere: a thread holding the lock and waiting for the database, while
/// another held the database and waited for the lock, ended in "database is locked".
pub(crate) fn in_mls_transaction<T>(storage: &Storage, name: &str, f: impl FnOnce(&Storage) -> OmniResult<T>) -> OmniResult<T> {
    let _one_at_a_time = ENGINE.lock();
    migrate_mls_tables(storage)?;
    storage.with_immediate_transaction(name, f)
}

/// The databases whose MLS tables this process has brought up to date.
static MLS_MIGRATED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

/// Bring the MLS tables up to date once per database and process, BEFORE any transaction of ours
/// begins: the migrations want a connection of their own (a keyed one: the database is
/// SQLCipher-encrypted, docs/decisions/db-at-rest-encryption.md), and run inside our open write
/// transaction they waited on it — the first MLS step after a start that came inside one (a Commit
/// taken in, a line sent) failed with "database is locked" (blind audit 2026-11-13, pass 18).
fn migrate_mls_tables(storage: &Storage) -> OmniResult<()> {
    let db = storage.db_path().to_path_buf();
    let key_text = db.to_string_lossy().to_string();
    let done = MLS_MIGRATED.get_or_init(Default::default).lock().map(|done| done.contains(&key_text)).unwrap_or(false);
    if done {
        return Ok(());
    }
    if storage.in_transaction() {
        return Err(OmniError::Internal("the MLS tables are brought up to date outside a transaction".into()));
    }
    let mut conn = Storage::open_keyed_connection(&db)?;
    Engine::<ProtectedCodec>::migrate(&mut conn).map_err(map_group_error)?;
    if let Ok(mut done) = MLS_MIGRATED.get_or_init(Default::default).lock() {
        done.insert(key_text);
    }
    Ok(())
}

/// Tests: this process has not brought `storage`'s MLS tables up to date (as after a start).
#[cfg(test)]
pub(crate) fn forget_mls_migrated_for_tests(storage: &Storage) {
    if let Ok(mut done) = MLS_MIGRATED.get_or_init(Default::default).lock() {
        done.remove(&storage.db_path().to_string_lossy().to_string());
    }
}

/// How many people (accounts, not devices) among `members` pass `keep`.
fn people_count(members: &[GroupMemberRecord], keep: impl Fn(&GroupMemberRecord) -> bool) -> usize {
    let mut accounts = std::collections::HashSet::new();
    members
        .iter()
        .filter(|m| keep(m))
        .filter(|m| m.account_id.is_empty() || accounts.insert(m.account_id.clone()))
        .count()
}

pub(crate) fn group_id_bytes(group_id: &str) -> OmniResult<Vec<u8>> {
    omni_protocol::validate_group_id_shape(group_id)?;
    hex::decode(group_id).map_err(|e| OmniError::InvalidInput(format!("group id: {e}")))
}

/// A fresh group message id (for the history packet, `app_group_history`).
pub(crate) fn random_group_message_id() -> String {
    random_id("g-")
}

fn random_id(prefix: &str) -> String {
    let bytes = omni_crypto::generate_group_key();
    format!("{prefix}{}", hex::encode(&bytes[..12]))
}

/// The mesh node of a member (its account key → node address), hex.
fn node_hex_of(member_pub: &str) -> String {
    let Some(key) = B64
        .decode(member_pub)
        .ok()
        .and_then(|raw| <[u8; 32]>::try_from(raw.as_slice()).ok())
        .and_then(|raw| ed25519_dalek::VerifyingKey::from_bytes(&raw).ok())
    else {
        return String::new();
    };
    hex::encode(omni_mesh_runtime::MeshNodeAddress::from_pubkey(&key).as_bytes())
}

pub(crate) fn node_from_hex(hex_str: &str) -> Option<omni_mesh_runtime::MeshNodeAddress> {
    let bytes: [u8; 20] = hex::decode(hex_str).ok()?.try_into().ok()?;
    Some(omni_mesh_runtime::MeshNodeAddress::from_bytes(bytes))
}

/// What our own Commit did — kept per epoch so a Commit that loses its epoch can be re-issued.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum OwnChange {
    /// We added these invitees (by their 1:1 dialog): re-invite with a fresh KeyPackage.
    Add { dialog_keys: Vec<String> },
    Remove { member_pubs: Vec<String> },
    Rename { name: String },
    Admins { admins: Vec<String> },
    /// The group's picture (base64 JPEG) set, or cleared.
    Avatar { avatar: Option<String> },
    /// Newcomers get the recent history, or not.
    NewcomerHistory { on: bool },
    /// We put back a member whose branch lost (`app_group_branches`). Not re-issued: if this Commit
    /// loses its epoch, the member asks again.
    PutBack { member_pub: String },
    /// Our leaf took our card with its device chain (`refresh_own_cards_in_groups`). Not re-issued:
    /// the next sweep sees the leaf without it and does it again.
    RefreshCard,
}

// --- the shell-facing API ----------------------------------------------------------------------

impl OmniApp {
    /// Create a group with us as its only member and admin, then invite `dialog_keys` (contacts).
    pub fn create_group(&self, name: &str) -> OmniResult<GroupSummary> {
        let _ = self.ensure_runtime_profile_active("create_group")?;
        let name = name.trim();
        if name.is_empty() || name.len() > omni_protocol::GROUP_NAME_MAX_BYTES {
            return Err(OmniError::InvalidInput("group name must be 1..128 bytes".into()));
        }
        let gid = omni_crypto::generate_group_key()[..16].to_vec();
        let group_id = hex::encode(&gid);
        let now = current_unix_ms();
        // The group's MLS state and its row here as one: apart, a failure between them left MLS state
        // of a group nobody sees (blind audit 2026-11-13, pass 18).
        in_mls_transaction(&self.storage, "group created", |storage| {
            with_engine(storage, |engine| engine.create_group(&gid, name))?;
            let (_, me) = local_mls_identity(storage)?;
            storage.upsert_group(&GroupRecord {
                group_id: group_id.clone(),
                name: name.to_string(),
                my_member_pub: me.member_pub,
                my_status: "member".into(),
                dialog_key: group_dialog_key(&group_id),
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
            })?;
            let view = with_engine(storage, |engine| engine.view(&gid))?;
            apply_view(storage, &group_id, &view, now)?;
            insert_system_line(storage, &group_id, &format!("Группа «{name}» создана"), now)
        })?;
        self.resubscribe_group(&group_id);
        self.group_summary(&group_id)?
            .ok_or_else(|| OmniError::Internal("group vanished after create".into()))
    }

    pub fn list_group_summaries(&self) -> OmniResult<Vec<GroupSummary>> {
        let _ = self.ensure_runtime_profile_active("list_groups")?;
        let mut out = Vec::new();
        for record in self.storage.list_groups()? {
            if let Some(summary) = self.group_summary(&record.group_id)? {
                out.push(summary);
            }
        }
        out.sort_by_key(|summary| std::cmp::Reverse(summary.last_activity_unix_ms));
        Ok(out)
    }

    /// The people of the group: one row per account — each device is an MLS member of its own
    /// (docs/decisions/multi-device-equal-nodes.md), but it is one person. The row is an admin if any
    /// of their devices is, a member while any is, and "me" for all of ours.
    pub fn group_members_view(&self, group_id: &str) -> OmniResult<Vec<GroupMemberView>> {
        let _ = self.ensure_runtime_profile_active("group_members_view")?;
        let group = self.storage.get_group(group_id)?;
        let my_pub = group.as_ref().map(|g| g.my_member_pub.clone()).unwrap_or_default();
        let my_account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let mut people: Vec<GroupMemberView> = Vec::new();
        for m in self.storage.list_group_members(group_id)? {
            let is_me = m.member_pub == my_pub || (!m.account_id.is_empty() && m.account_id == my_account);
            // Contact first (the name WE gave, its photo over 1:1), then the member's own card, then
            // the name its MLS credential carried when it joined.
            let contact = if is_me || m.account_id.is_empty() {
                None
            } else {
                self.storage
                    .find_contact_by_remote_account_id(&omni_identity::AccountId(m.account_id.clone()))
                    .ok()
                    .flatten()
            };
            let card = if is_me || m.account_id.is_empty() {
                None
            } else {
                self.storage.get_group_member_profile(&m.account_id).ok().flatten()
            };
            let (display_name, avatar_path) = match (&contact, &card) {
                (Some(contact), _) if !contact.petname.trim().is_empty() => {
                    (contact.petname.clone(), contact.public_avatar_image_path.clone().or_else(|| card.as_ref().and_then(|c| c.avatar_path.clone())))
                }
                (_, Some(card)) => (card.display_name.clone(), card.avatar_path.clone()),
                _ => (m.display_name.clone(), None),
            };
            let row = GroupMemberView {
                is_me,
                member_pub: m.member_pub,
                display_name,
                role: m.role,
                status: m.status,
                account_id: m.account_id,
                avatar_path,
            };
            let same = people
                .iter_mut()
                .find(|p| !row.account_id.is_empty() && p.account_id == row.account_id);
            match same {
                Some(person) => {
                    if row.role == "admin" {
                        person.role = row.role;
                    }
                    if row.status == "member" && person.status != "member" {
                        person.status = row.status;
                        person.member_pub = row.member_pub;
                    }
                    person.is_me |= row.is_me;
                }
                None => people.push(row),
            }
        }
        Ok(people)
    }

    /// Every device of the person `member_pub` belongs to, still in the group (a person is removed or
    /// made an admin with all their devices).
    fn person_pubs(&self, group_id: &str, member_pub: &str) -> OmniResult<Vec<String>> {
        let members = self.storage.list_group_members(group_id)?;
        let account = members
            .iter()
            .find(|m| m.member_pub == member_pub)
            .map(|m| m.account_id.clone())
            .filter(|account| !account.is_empty());
        let mut pubs: Vec<String> = match account {
            Some(account) => members
                .into_iter()
                .filter(|m| m.account_id == account && m.status == "member")
                .map(|m| m.member_pub)
                .collect(),
            None => Vec::new(),
        };
        if !pubs.iter().any(|p| p == member_pub) {
            pubs.push(member_pub.to_string());
        }
        Ok(pubs)
    }

    /// The group thread with authors (the generic chat list sees only the text).
    pub fn list_group_messages(&self, group_id: &str) -> OmniResult<Vec<GroupMessageView>> {
        let _ = self.ensure_runtime_profile_active("list_group_messages")?;
        let group = self
            .storage
            .get_group(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let meta: HashMap<String, GroupMessageMeta> = self
            .storage
            .list_group_message_meta(group_id)?
            .into_iter()
            .map(|m| (m.message_id.clone(), m))
            .collect();
        let db_path = self.storage.db_path().to_string_lossy().into_owned();
        // Our other devices' leaves: what they wrote is ours, as a sent copy is in a 1:1 chat.
        let my_account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let members = self.storage.list_group_members(group_id)?;
        // "<name> · <owner's agent>": whose agent wrote it (docs/decisions/omni-workspace-git.md section 5).
        let agent_labels = crate::app_workspace::agents::labels(&self.storage, &members);
        let names_by_account: HashMap<String, String> =
            members.iter().map(|m| (m.account_id.clone(), m.display_name.clone())).collect();
        let own_pubs: std::collections::HashSet<String> = members
            .into_iter()
            .filter(|m| !m.account_id.is_empty() && m.account_id == my_account)
            .map(|m| m.member_pub)
            .collect();
        Ok(self
            .storage
            .list_messages_for_dialog(&group.dialog_key)?
            .into_iter()
            .map(|record| {
                let m = meta.get(&record.id);
                let is_mine = matches!(record.direction, omni_types::ChatDirection::Outgoing)
                    || m.and_then(|m| m.sender_pub.as_ref()).is_some_and(|pub_key| own_pubs.contains(pub_key));
                GroupMessageView {
                    sender_pub: m.and_then(|m| m.sender_pub.clone()),
                    sender_name: m
                        .map(|m| {
                            let label = m.sender_pub.as_ref().and_then(|p| agent_labels.get(p)).map(String::as_str).unwrap_or("");
                            format!("{}{label}", m.sender_name)
                        })
                        .unwrap_or_default(),
                    kind: m.map(|m| m.kind.clone()).unwrap_or_else(|| "text".into()),
                    is_mine,
                    status: format!("{:?}", record.status).to_lowercase(),
                    message_id: record.id,
                    body: record.body,
                    created_at_unix_ms: record.created_at_unix_ms,
                    reply: record.reply,
                    pinned: record.pinned,
                    reactions: record
                        .reactions
                        .into_iter()
                        .map(|mut reaction| {
                            for who in reaction.who.iter_mut() {
                                if let Some(name) = names_by_account.get(who.as_str()) {
                                    *who = name.clone();
                                }
                            }
                            reaction
                        })
                        .collect(),
                    attachment: record.attachment.map(|mut attachment| {
                        // How many members it is coming from right now (stage 3: several at once).
                        attachment.source_count =
                            crate::app_group_attachments::group_pull_sources(&db_path, &attachment.message_id);
                        attachment
                    }),
                }
            })
            .collect())
    }

    pub fn send_group_text(&self, group_id: &str, body: &str) -> OmniResult<()> {
        self.send_group_line(group_id, body, None, &[])
    }

    /// A text line that answers `reply_to` (a line of the same group) and names members: `picked` are
    /// the accounts chosen in the suggestion list (`app_group_mentions`).
    pub fn send_group_line(&self, group_id: &str, body: &str, reply_to: Option<&str>, picked: &[String]) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("send_group_text")?;
        let body = body.trim();
        if body.is_empty() {
            return Err(OmniError::InvalidInput("message body must not be empty".into()));
        }
        // The members' receive check (`GROUP_TEXT_MAX_BYTES`) on the send side too, as 1:1 does
        // (`send_text`): past it the line would go out and be dropped by every member.
        if body.len() > omni_protocol::GROUP_TEXT_MAX_BYTES {
            return Err(OmniError::InvalidInput(format!(
                "message body exceeds the {} byte limit; send large content as an attachment instead",
                omni_protocol::GROUP_TEXT_MAX_BYTES
            )));
        }
        let group = self.active_group(group_id)?;
        let now = current_unix_ms();
        let (_, me) = local_mls_identity(&self.storage)?;
        let mut payload = GroupAppPayloadV1 {
            message_id: random_id("g-"),
            kind: GroupAppKindV1::Text,
            body: body.to_string(),
            authored_at_unix_ms: now,
            signature_b64: None,
            reply_to: reply_to.map(str::to_string),
            mentions: crate::app_group_mentions::resolve_mentions(&self.storage.list_group_members(group_id)?, body, picked)?,
        };
        self.storage.remember_message_mentions(&payload.message_id, &payload.mentions)?;
        // Before the signed line is kept: a newcomer's history reads it from here.
        self.storage.remember_message_reply(&payload.message_id, &group.dialog_key, reply_to)?;
        payload.reply_to = self.storage.message_reply_to(&payload.message_id)?;
        crate::app_group_history::sign_own_line(&self.storage, group_id, &mut payload, &me)?;
        // The line in our thread and in the group's queue, as one (blind audit 2026-11-11, pass 13).
        act_and_say(&self.storage, group_id, &payload, |storage| {
            storage.insert_group_outgoing_message(&payload.message_id, &group.dialog_key, body, now)?;
            storage.insert_group_message_meta(
                group_id,
                &GroupMessageMeta {
                    message_id: payload.message_id.clone(),
                    sender_pub: Some(me.member_pub.clone()),
                    sender_name: me.display_name.clone(),
                    kind: "text".into(),
                },
            )
        })?;
        self.flush_group_feed();
        Ok(())
    }

    /// A file or photo into the group: the offer rides the group as an MLS message, the bytes stay
    /// here until members pull them (`app_group_attachments`).
    pub fn send_group_attachment(
        &self,
        group_id: &str,
        body: &str,
        source_file_path: &str,
        display_file_name: Option<&str>,
        as_photo: bool,
        recording: Option<omni_types::AttachmentPresentationKindV1>,
        reply_to: Option<&str>,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("send_group_attachment")?;
        let source_file_path = source_file_path.trim();
        if source_file_path.is_empty() {
            return Err(OmniError::InvalidInput("source_file_path must not be empty".into()));
        }
        let body = body.trim();
        if body.len() > omni_protocol::CHAT_BODY_MAX_BYTES {
            return Err(OmniError::InvalidInput("caption is too long".into()));
        }
        let group = self.active_group(group_id)?;
        let message_id = random_id("g-");
        let now = current_unix_ms();
        self.storage.remember_message_reply(&message_id, &group.dialog_key, reply_to)?;
        // The file first: one this app cannot take (or a picture it cannot read) fails here,
        // before the thread shows a message.
        let staged = crate::stage_outgoing_photo_if_asked(&self.storage, &message_id, source_file_path, as_photo, recording)?;
        let runtime = crate::register_outgoing_attachment(
            &self.storage,
            &message_id,
            &group.dialog_key,
            source_file_path,
            display_file_name,
            staged,
            omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            now,
        )?;
        // A big file is still being read: the line shows now, the preparation sends its offer
        // (`send_prepared_group_attachment_offer`).
        let preparing = runtime.attachment.state == omni_types::AttachmentTransferState::Preparing;
        let (_, me) = local_mls_identity(&self.storage)?;
        // The offer on the group's queue and our line with its author, as one: the offer was queued
        // first, and a failure (or a crash) before the line sent the members a file its sender's
        // thread did not show (blind audit 2026-11-13, pass 19).
        in_mls_transaction(&self.storage, "group attachment", |storage| {
            if !preparing {
                send_group_attachment_offer(storage, group_id, &runtime, body, now)?;
            }
            storage.insert_group_outgoing_message(&message_id, &group.dialog_key, body, now)?;
            storage.insert_group_message_meta(
                group_id,
                &GroupMessageMeta {
                    message_id: message_id.clone(),
                    sender_pub: Some(me.member_pub.clone()),
                    sender_name: me.display_name.clone(),
                    kind: "attachment".into(),
                },
            )
        })?;
        self.flush_group_feed();
        Ok(())
    }

    /// Invite contacts (by their 1:1 dialog) — any member may.
    pub fn invite_contacts_to_group(&self, group_id: &str, dialog_keys: &[String]) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("invite_contacts_to_group")?;
        let group = self.active_group(group_id)?;
        let (_, me) = local_mls_identity(&self.storage)?;
        for dialog_key in dialog_keys {
            let invite = GroupInviteV2 {
                version: GROUP_WIRE_VERSION_V2,
                group_id: group_id.to_string(),
                name: group.name.clone(),
                inviter_name: me.display_name.clone(),
                renew: false,
                authored_at_unix_ms: current_unix_ms(),
            };
            {
                // The public methods send under the session lock, like every other sender (B05,
                // audit 2026-11-09 #6); the retry on a stale write stays as a second line.
                let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
                send_control_1to1(&self.storage, &self.transport, dialog_key, GroupControlKind::Invite, &invite)?;
            }
            let now = current_unix_ms();
            self.storage.record_group_sent_invite(group_id, dialog_key, now)?;
            // Only we see this line: the invitation goes over 1:1, not through the group.
            let name = contact_name_for_dialog(&self.storage, dialog_key);
            insert_system_line(&self.storage, group_id, &format!("Вы пригласили: {name}"), now)?;
        }
        Ok(())
    }

    /// Our still-unanswered invitations for a group. One whose contact has meanwhile joined (someone
    /// else's invitation got there first) is dropped here — it is answered.
    pub fn list_group_sent_invites(&self, group_id: &str) -> OmniResult<Vec<GroupSentInviteView>> {
        let _ = self.ensure_runtime_profile_active("list_group_sent_invites")?;
        let members: std::collections::HashSet<String> = self
            .storage
            .list_group_members(group_id)?
            .into_iter()
            .filter(|m| m.status == "member")
            .map(|m| m.account_id)
            .collect();
        let mut out = Vec::new();
        for (dialog_key, sent_at) in self.storage.list_group_sent_invites(group_id)? {
            let contact = self.storage.resolve_contact_for_dialog(&dialog_key)?;
            let joined = contact
                .as_ref()
                .and_then(|c| c.remote_account_id.as_ref())
                .is_some_and(|a| members.contains(&a.0));
            if joined {
                let _ = self.storage.take_group_sent_invite(group_id, &dialog_key);
                continue;
            }
            out.push(GroupSentInviteView {
                display_name: contact_name_for_dialog(&self.storage, &dialog_key),
                dialog_key,
                sent_at_unix_ms: sent_at,
            });
        }
        Ok(out)
    }

    /// Withdraw an invitation we sent: an accept that arrives later is ignored (`add_invitee`), and
    /// the invitee is told, so the offer disappears there instead of hanging at "joining".
    pub fn cancel_group_invite(&self, group_id: &str, dialog_key: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("cancel_group_invite")?;
        if !self.storage.take_group_sent_invite(group_id, dialog_key)? {
            return Err(OmniError::InvalidInput("no such invitation".into()));
        }
        let withdraw = GroupInviteDeclineV2 { version: GROUP_WIRE_VERSION_V2, group_id: group_id.to_string() };
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let _ = send_control_1to1(&self.storage, &self.transport, dialog_key, GroupControlKind::InviteDecline, &withdraw);
        let name = contact_name_for_dialog(&self.storage, dialog_key);
        insert_system_line(&self.storage, group_id, &format!("Приглашение отменено: {name}"), current_unix_ms())?;
        Ok(())
    }

    pub fn list_group_invites(&self) -> OmniResult<Vec<GroupInviteView>> {
        let _ = self.ensure_runtime_profile_active("list_group_invites")?;
        let mut out = Vec::new();
        for i in self.storage.list_open_group_invites()? {
            // An accepted invitation is done once the group is here — in it, or out of it again
            // (removed, deleted by an admin): only a Welcome still on its way keeps it open.
            if i.status == "accepted" && self.storage.get_group(&i.group_id)?.is_some() {
                continue;
            }
            out.push(GroupInviteView {
                group_id: i.group_id,
                name: i.name,
                inviter_name: i.inviter_name,
                inviter_dialog_key: i.inviter_dialog_key,
                received_at_unix_ms: i.received_at_unix_ms,
                status: i.status,
            });
        }
        Ok(out)
    }

    /// Accept an invitation: hand the inviter a fresh KeyPackage; the Welcome comes back.
    pub fn accept_group_invite(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("accept_group_invite")?;
        let invite = self
            .storage
            .get_group_invite(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("no such invitation".into()))?;
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        accept_invite_via(&self.storage, &self.transport, &invite)
    }

    pub fn decline_group_invite(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("decline_group_invite")?;
        let invite = self
            .storage
            .get_group_invite(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("no such invitation".into()))?;
        self.storage.upsert_group_invite(&GroupInviteRecord { status: "declined".into(), ..invite.clone() })?;
        let decline = GroupInviteDeclineV2 { version: GROUP_WIRE_VERSION_V2, group_id: group_id.to_string() };
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let _ = send_control_1to1(
            &self.storage,
            &self.transport,
            &invite.inviter_dialog_key,
            GroupControlKind::InviteDecline,
            &decline,
        );
        Ok(())
    }

    /// Leave and delete the chat. Still in: tell the group (an admin removes us with a Commit) and
    /// drop the MLS state. Then the conversation goes from this device — a left group is not kept
    /// as a dead chat. A group we were removed from, or one an admin deleted, is only deleted here.
    /// The last admin of a group with others must name another admin, or delete it for everyone.
    pub fn leave_group(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("leave_group")?;
        let group = self
            .storage
            .get_group(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        if group.my_status == "member" {
            let gid = group_id_bytes(group_id)?;
            with_engine(&self.storage, |engine| engine.can_leave(&gid))?;
            self.say_to_group(group_id, GroupAppKindV1::Leave)?;
        }
        self.delete_group_here(&group)
    }

    /// An admin deletes the group for everyone: one signed "deleted" message to the members (they
    /// stop, and keep a read-only chat until they delete it), then it goes from here as on leave.
    pub fn delete_group_for_everyone(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("delete_group_for_everyone")?;
        let group = self.active_group(group_id)?;
        let i_am_admin = self
            .storage
            .list_group_members(group_id)?
            .iter()
            .any(|m| m.member_pub == group.my_member_pub && m.role == "admin");
        if !i_am_admin {
            return Err(OmniError::InvalidInput("only an admin deletes the group for everyone".into()));
        }
        self.say_to_group(group_id, GroupAppKindV1::Dissolve)?;
        self.delete_group_here(&group)
    }

    fn say_to_group(&self, group_id: &str, kind: GroupAppKindV1) -> OmniResult<()> {
        let payload = GroupAppPayloadV1 {
            message_id: random_id("g-"),
            kind,
            body: String::new(),
            authored_at_unix_ms: current_unix_ms(),
            signature_b64: None,
            reply_to: None,
            mentions: Vec::new(),
        };
        send_app_payload(&self.storage, group_id, &payload)?;
        self.flush_group_feed();
        Ok(())
    }

    fn delete_group_here(&self, group: &GroupRecord) -> OmniResult<()> {
        crate::app_group_calls::end_group_call_here(&self.storage.db_path().to_string_lossy(), &group.group_id);
        let gid = group_id_bytes(&group.group_id)?;
        with_engine(&self.storage, |engine| engine.delete_group(&gid))?;
        // Out of the group: stop taking its frames (re-subscribing with no members, as before,
        // kept every frame of the left group flowing into our inbox).
        if let Some(mesh) = self.mesh_runtime() {
            mesh.unsubscribe_group_destination(&group.group_id);
        }
        forget_group_chat(&self.storage, group)
    }

    pub fn remove_group_member(&self, group_id: &str, member_pub: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("remove_group_member")?;
        self.active_group(group_id)?;
        let gid = group_id_bytes(group_id)?;
        let pubs = self.person_pubs(group_id, member_pub)?;
        self.own_commit(group_id, OwnChange::Remove { member_pubs: pubs.clone() }, |engine| engine.remove_members(&gid, &pubs))
    }

    pub fn rename_group(&self, group_id: &str, name: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("rename_group")?;
        let name = name.trim().to_string();
        if name.is_empty() || name.len() > omni_protocol::GROUP_NAME_MAX_BYTES {
            return Err(OmniError::InvalidInput("group name must be 1..128 bytes".into()));
        }
        self.active_group(group_id)?;
        let gid = group_id_bytes(group_id)?;
        let new_name = name.clone();
        self.own_commit(group_id, OwnChange::Rename { name }, |engine| engine.update_meta(&gid, |m| m.name = new_name))
    }

    /// An admin sets the group's picture (`Some(path)` of an image file — normalized to a small
    /// JPEG here) or clears it (`None`). It rides in the group's MLS meta like the name: every
    /// member and every newcomer gets it, and only an admin's Commit may change it.
    pub fn set_group_avatar(&self, group_id: &str, source_path: Option<&str>) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_group_avatar")?;
        self.active_group(group_id)?;
        let avatar = match source_path {
            Some(path) => {
                let (jpeg, _) = omni_storage::normalize_group_avatar_image(std::path::Path::new(path))?;
                Some(B64.encode(jpeg))
            }
            None => None,
        };
        let gid = group_id_bytes(group_id)?;
        let wanted = avatar.clone();
        self.own_commit(group_id, OwnChange::Avatar { avatar }, |engine| engine.update_meta(&gid, |m| m.avatar = wanted))
    }

    /// An admin decides whether a newcomer gets the recent history (on by default). It rides in the
    /// group's MLS meta: only an admin's Commit changes it, every member sees the same switch.
    pub fn set_group_newcomer_history(&self, group_id: &str, on: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_group_newcomer_history")?;
        self.active_group(group_id)?;
        let gid = group_id_bytes(group_id)?;
        self.own_commit(group_id, OwnChange::NewcomerHistory { on }, |engine| engine.update_meta(&gid, |m| m.newcomer_history = on))
    }

    pub fn set_group_admin(&self, group_id: &str, member_pub: &str, admin: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_group_admin")?;
        self.active_group(group_id)?;
        // An agent is never an admin (docs/decisions/omni-workspace-git.md section 5).
        let account = self.storage.list_group_members(group_id)?.into_iter().find(|m| m.member_pub == member_pub).map(|m| m.account_id);
        if admin && account.is_some_and(|a| crate::app_workspace::agents::is_agent(&self.storage, &a)) {
            return Err(OmniError::InvalidInput("агент не может быть админом".into()));
        }
        let gid = group_id_bytes(group_id)?;
        let targets = self.person_pubs(group_id, member_pub)?;
        // The admins after the change are what the Commit makes them, and they are only known inside it.
        let after = std::cell::RefCell::new(Vec::new());
        self.own_commit_with(
            group_id,
            |_| OwnChange::Admins { admins: after.borrow().clone() },
            |engine| {
                engine.update_meta(&gid, |m| {
                    m.admins.retain(|a| !targets.contains(a));
                    if admin {
                        m.admins.extend(targets.iter().cloned());
                    }
                    *after.borrow_mut() = m.admins.clone();
                })
            },
        )
    }

    pub(crate) fn active_group(&self, group_id: &str) -> OmniResult<GroupRecord> {
        let group = self
            .storage
            .get_group(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        if group.my_status != "member" {
            return Err(OmniError::InvalidInput("not an active member of the group".into()));
        }
        Ok(group)
    }

    /// Our Commit and its bookkeeping as one transaction ([`with_engine`]), then out to the members.
    fn own_commit(
        &self,
        group_id: &str,
        change: OwnChange,
        make: impl FnOnce(&Engine<'_, ProtectedCodec>) -> omni_group_mls::Result<Vec<u8>>,
    ) -> OmniResult<()> {
        self.own_commit_with(group_id, move |_| change, make)
    }

    fn own_commit_with(
        &self,
        group_id: &str,
        change: impl FnOnce(&[u8]) -> OwnChange,
        make: impl FnOnce(&Engine<'_, ProtectedCodec>) -> omni_group_mls::Result<Vec<u8>>,
    ) -> OmniResult<()> {
        in_mls_transaction(&self.storage, "group_own_commit", |storage| {
            let commit = with_engine(storage, make)?;
            own_commit_bookkeeping(storage, group_id, &commit, &change(&commit))
        })?;
        self.resubscribe_group(group_id);
        self.flush_group_feed();
        Ok(())
    }

    fn group_summary(&self, group_id: &str) -> OmniResult<Option<GroupSummary>> {
        let Some(group) = self.storage.get_group(group_id)? else {
            return Ok(None);
        };
        // Deleted by the user; only a farewell still waiting to go out keeps the row.
        if group.my_status == "deleted" {
            return Ok(None);
        }
        let members = self.storage.list_group_members(group_id)?;
        let my_role = members
            .iter()
            .find(|m| m.member_pub == group.my_member_pub)
            .map(|m| m.role.clone())
            .unwrap_or_else(|| "member".into());
        let (epoch, newcomer_history) = match group_id_bytes(group_id) {
            Ok(gid) if group.my_status == "member" => with_engine(&self.storage, |engine| engine.view(&gid))
                .map(|v| (v.epoch, v.newcomer_history))
                .unwrap_or((0, true)),
            _ => (0, true),
        };
        // Only the latest line: the chat list polls this, a whole-history read per group per tick
        // grew with every message.
        let latest = self.storage.get_latest_message_for_dialog(&group.dialog_key)?;
        let latest_meta = match &latest {
            Some(m) => self.storage.get_group_message_meta(group_id, &m.id)?,
            None => None,
        };
        let (preview, activity) = latest
            .as_ref()
            .map(|m| {
                // A file without a caption reads as the 1:1 list does: «Фото», «Файл: name».
                let text = match &m.attachment {
                    Some(file) if m.body.trim().is_empty() => match file.presentation.as_ref().map(|shown| shown.kind) {
                        Some(omni_types::AttachmentPresentationKindV1::Photo) => "Фото".to_string(),
                        Some(omni_types::AttachmentPresentationKindV1::Voice) => "Голосовое сообщение".to_string(),
                        Some(omni_types::AttachmentPresentationKindV1::VideoNote) => "Видеосообщение".to_string(),
                        None => format!("Файл: {}", file.file_name),
                    },
                    _ => m.body.clone(),
                };
                let preview = match latest_meta.as_ref() {
                    // Own lines read "Вы: …" in the list, like the 1:1 preview, not our own name.
                    Some(meta) if meta.kind != "system" && matches!(m.direction, omni_types::ChatDirection::Outgoing) => {
                        format!("Вы: {text}")
                    }
                    Some(meta) if meta.kind != "system" => format!("{}: {text}", meta.sender_name),
                    _ => text,
                };
                (preview, m.created_at_unix_ms)
            })
            .unwrap_or((String::new(), group.created_at_unix_ms));
        let unread_count = match &latest {
            Some(_) => self.storage.count_unread_in_dialog(&group.dialog_key)?,
            None => 0,
        };
        Ok(Some(GroupSummary {
            group_id: group.group_id,
            name: group.name,
            dialog_key: group.dialog_key,
            my_role,
            my_status: group.my_status,
            member_count: people_count(&members, |_| true),
            active_member_count: people_count(&members, |m| m.status == "member"),
            epoch,
            last_message_preview: preview,
            last_activity_unix_ms: activity,
            unread_count,
            avatar_path: self.storage.group_avatar_image_path(group_id).unwrap_or(None),
            newcomer_history,
            is_workspace: !self.storage.list_workspace_repo_names(group_id)?.is_empty(),
        }))
    }

    /// (Re)subscribe the mesh to the group's destination with the current members' nodes.
    fn resubscribe_group(&self, group_id: &str) {
        if let Some(mesh) = self.mesh_runtime() {
            resubscribe_group_via(&self.storage, &mesh, group_id);
        }
    }

    /// Subscribe every group on mesh start (called from start_mesh).
    pub(crate) fn resubscribe_all_groups(&self) {
        if let Some(mesh) = self.mesh_runtime() {
            resubscribe_all_groups_via(&self.storage, &mesh);
        }
    }

    /// Publish what is queued for the group feeds, if the mesh is up.
    pub(crate) fn flush_group_feed(&self) {
        if let Some(mesh) = self.mesh_runtime() {
            flush_group_feed_outbox(&self.storage, &mesh);
        }
    }
}

// --- free functions: the paths without `&OmniApp` (1:1 receive, mesh wire worker) -------------

/// Groups whose MLS state is here but whose app row is not — an Oct 3 build opened on the database
/// re-created `group_ops`, and the next start's migration dropped `groups`/`group_members` on it
/// (docs/bugs/old-build-wiped-groups.md). The row and the members come back from the MLS view; our
/// feed counts on past what it may already have carried. Run at mesh start, before subscribing.
/// `local_node` is the node this profile runs as: our feed in a group is named by it. Returns how
/// many came back.
pub(crate) fn restore_groups_without_rows(
    storage: &Storage,
    local_node: &omni_mesh_runtime::MeshNodeAddress,
) -> usize {
    let Ok(candidates) = storage.list_group_ids_without_row() else {
        return 0;
    };
    if candidates.is_empty() {
        return 0;
    }
    let Ok((_, me)) = local_mls_identity(storage) else {
        return 0;
    };
    let mut restored = 0;
    for group_id in candidates {
        let Ok(gid) = group_id_bytes(&group_id) else {
            continue;
        };
        let Ok(view) = with_engine(storage, |engine| engine.view(&gid)) else {
            continue; // no MLS state either: nothing to bring back
        };
        if !view.members.iter().any(|m| m.is_me) {
            continue;
        }
        let now = current_unix_ms();
        let record = GroupRecord {
            group_id: group_id.clone(),
            name: view.name.clone(),
            my_member_pub: me.member_pub.clone(),
            my_status: if view.active { "member".into() } else { "removed".into() },
            dialog_key: group_dialog_key(&group_id),
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        };
        if storage.upsert_group(&record).is_err() {
            continue;
        }
        let feed_id = omni_mesh_runtime::MeshRuntime::group_feed_id_for(
            "omni.group.feed",
            &group_id,
            &me.member_pub,
            local_node,
        );
        let floor = storage.group_feed_seq_floor(&group_id, &feed_id).unwrap_or(0);
        let _ = storage.set_group_feed_seq(&group_id, floor);
        // No members before: apply_view writes no "X в группе" lines for them.
        let _ = apply_view(storage, &group_id, &view, now);
        crate::omni_app_trace_log(
            "group/restore",
            &format!("group={group_id} name={:?} members={} feed_seq_from={floor}", view.name, view.members.len()),
        );
        crate::record_group_changed_event(&storage.db_path().to_string_lossy(), &group_id);
        restored += 1;
    }
    restored
}

/// Keep the mesh subscribed to every group we are in, with the current members. Run on mesh start
/// and every autoconnect tick: a group joined from a Welcome (the 1:1 receive path, no mesh handle
/// there) gets its feed subscription within a tick instead of on our first own message — frames
/// that arrive before the subscription are only held by the mesh.
///
/// A group we are no longer in (removed, dissolved) is unsubscribed on the same tick: those used to
/// keep their subscription — frames of a group we are out of kept coming (A10, audit 2026-11-09 #6).
pub(crate) fn resubscribe_all_groups_via(storage: &Storage, mesh: &omni_mesh_runtime::MeshRuntime) {
    if let Ok(groups) = storage.list_groups() {
        for group in &groups {
            match group.my_status.as_str() {
                "member" => resubscribe_group_via(storage, mesh, &group.group_id),
                "removed" | "dissolved" => mesh.unsubscribe_group_destination(&group.group_id),
                _ => {}
            }
        }
    }
}

fn resubscribe_group_via(storage: &Storage, mesh: &omni_mesh_runtime::MeshRuntime, group_id: &str) {
    let nodes: Vec<omni_mesh_runtime::MeshNodeAddress> = storage
        .list_group_members(group_id)
        .unwrap_or_default()
        .iter()
        .filter(|m| m.status == "member")
        .filter_map(|m| node_from_hex(&m.node_hint_hex))
        .collect();
    mesh.subscribe_group_destination(group_id, nodes);
}

/// The chat of a group the user left or deleted goes from this device: its lines, files, meta and
/// invitation. Whatever is still queued for its feed (the farewell, sent offline) keeps the row
/// until it is out — [`finish_deleted_groups`].
fn forget_group_chat(storage: &Storage, group: &GroupRecord) -> OmniResult<()> {
    storage.delete_dialog_history(&group.dialog_key)?;
    storage.delete_group_chat_rows(&group.group_id)?;
    if storage.count_group_feed_outbox(&group.group_id)? == 0 {
        storage.delete_group_rows(&group.group_id)?;
    } else {
        storage.upsert_group(&GroupRecord {
            my_status: "deleted".into(),
            updated_at_unix_ms: current_unix_ms(),
            ..group.clone()
        })?;
    }
    crate::record_group_changed_event(&storage.db_path().to_string_lossy(), &group.group_id);
    Ok(())
}

/// Drop the last rows of deleted groups whose farewell has gone out.
pub(crate) fn finish_deleted_groups(storage: &Storage, mesh: Option<&omni_mesh_runtime::MeshRuntime>) {
    let Ok(groups) = storage.list_groups() else {
        return;
    };
    for group in groups.iter().filter(|g| g.my_status == "deleted") {
        if storage.count_group_feed_outbox(&group.group_id).is_ok_and(|n| n == 0)
            && storage.delete_group_rows(&group.group_id).is_ok()
        {
            if let Some(mesh) = mesh {
                mesh.unsubscribe_group_destination(&group.group_id);
            }
        }
    }
}

/// Publish the queued feed messages (oldest first) on our durable feeds.
pub(crate) fn flush_group_feed_outbox(storage: &Storage, mesh: &omni_mesh_runtime::MeshRuntime) {
    publish_group_feed_outbox(storage, mesh);
    finish_deleted_groups(storage, Some(mesh));
}

fn publish_group_feed_outbox(storage: &Storage, mesh: &omni_mesh_runtime::MeshRuntime) {
    let Ok(queued) = storage.list_group_feed_outbox(64) else {
        return;
    };
    for (row, group_id, mls) in queued {
        let Ok(Some(group)) = storage.get_group(&group_id) else {
            let _ = storage.delete_group_feed_outbox(row);
            continue;
        };
        resubscribe_group_via(storage, mesh, &group_id);
        let item = GroupFeedItemV2 {
            version: GROUP_WIRE_VERSION_V2,
            group_id: group_id.clone(),
            mls_b64: B64.encode(&mls),
        };
        let Ok(item_json) = serde_json::to_string(&item) else {
            continue;
        };
        let frame = omni_transport::TransportRequestEnvelopeV1::group_feed("mls", item_json);
        let Ok(ciphertext) = serde_json::to_vec(&frame) else {
            continue;
        };
        let feed_id = omni_mesh_runtime::MeshRuntime::group_feed_id_for(
            "omni.group.feed",
            &group_id,
            &group.my_member_pub,
            &mesh.node_address(),
        );
        let Ok(seq) = storage.next_group_feed_seq(&group_id) else {
            continue;
        };
        match mesh.send_group_durable(&group_id, feed_id, seq, &ciphertext) {
            Ok(()) => {
                let _ = storage.delete_group_feed_outbox(row);
            }
            Err(error) => {
                crate::omni_app_trace_log("group/publish", &format!("group={group_id} failed: {error}"));
                return; // keep order: retry from this one on the next flush
            }
        }
    }
}

/// Encrypt an application payload for the group and queue it; remember it per epoch (re-sent if a
/// rollback takes that epoch away).
/// The offer of our file to the group, signed as our line.
fn send_group_attachment_offer(
    storage: &Storage,
    group_id: &str,
    runtime: &omni_storage::AttachmentRuntimeInfo,
    body: &str,
    authored_at_unix_ms: i64,
) -> OmniResult<()> {
    let message_id = &runtime.attachment.message_id;
    let offer = runtime.offer_for(message_id, body, authored_at_unix_ms);
    let (_, me) = local_mls_identity(storage)?;
    let mut payload = GroupAppPayloadV1 {
        message_id: message_id.clone(),
        kind: GroupAppKindV1::Attachment,
        body: serde_json::to_string(&offer).map_err(|e| OmniError::Internal(e.to_string()))?,
        authored_at_unix_ms,
        signature_b64: None,
        reply_to: None,
        mentions: Vec::new(),
    };
    omni_protocol::validate_group_app_payload_v1(&payload)?;
    crate::app_group_history::sign_own_line(storage, group_id, &mut payload, &me)?;
    send_app_payload(storage, group_id, &payload)
}

/// A group file whose preparation just ended: its offer goes out now (the line has been in the
/// thread since it was sent). The mesh tick publishes the feed.
pub(crate) fn send_prepared_group_attachment_offer(storage: &Storage, message_id: &str) -> OmniResult<()> {
    #[cfg(test)]
    {
        let db_path = storage.db_path().to_string_lossy().into_owned();
        let mut fail = crate::app_attachment_preparation::FAIL_PREPARED_OFFER.lock().unwrap();
        if let Some(at) = fail.iter().position(|p| *p == db_path) {
            fail.remove(at);
            return Err(OmniError::StorageLocked("test: the offer fails".into()));
        }
    }
    let runtime = storage
        .get_attachment_runtime_info(message_id)?
        .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?;
    let group = storage
        .get_group_by_dialog_key(&runtime.dialog_key)?
        .ok_or_else(|| OmniError::Storage(format!("no group for '{}'", runtime.dialog_key)))?;
    let message = storage.get_message(message_id)?;
    send_group_attachment_offer(storage, &group.group_id, &runtime, &message.body, message.created_at_unix_ms)
}

pub(crate) fn send_app_payload(storage: &Storage, group_id: &str, payload: &GroupAppPayloadV1) -> OmniResult<()> {
    act_and_say(storage, group_id, payload, |_| Ok(()))
}

#[cfg(test)]
thread_local! {
    /// Test hook: the group's queue refuses the next lines of this thread.
    pub(crate) static FAIL_GROUP_QUEUE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Do something here and say it to the group, as one: `here` (the local change) and the line's
/// place in the group's queue are written in one transaction — both, or neither and an error. They
/// were separate writes: a failure between them left the thing done here and never said, or said
/// and not done (blind audit 2026-11-11, pass 13, class 2; maintainers' decision 2026-11-11). The line
/// is sealed in the same transaction: the MLS engine shares the app's connection now (pass 17), so
/// a line not queued does not spend a generation either. `here` gets the storage inside the
/// transaction: it must not open another connection (`Storage::open`) — that one would wait on this.
pub(crate) fn act_and_say<T>(
    storage: &Storage,
    group_id: &str,
    payload: &GroupAppPayloadV1,
    here: impl FnOnce(&Storage) -> OmniResult<T>,
) -> OmniResult<T> {
    omni_protocol::validate_group_app_payload_v1(payload)?;
    let gid = group_id_bytes(group_id)?;
    let json = serde_json::to_string(payload).map_err(|e| OmniError::Internal(e.to_string()))?;
    let now = current_unix_ms();
    in_mls_transaction(storage, "group action and its line", |storage| {
        let (epoch, mls) = with_engine(storage, |engine| engine.encrypt(&gid, json.as_bytes()))?;
        let done = here(storage)?;
        #[cfg(test)]
        if FAIL_GROUP_QUEUE.with(|fail| fail.get()) {
            return Err(OmniError::Storage("test: the group's queue does not take the line".into()));
        }
        storage.record_group_outbox(group_id, epoch, &payload.message_id, &json, now)?;
        storage.enqueue_group_feed(group_id, &mls, now)?;
        Ok(done)
    })
}

/// Group-call control from the calls module (free fn: the ingest worker has no `OmniApp`).
pub(crate) fn publish_group_call_control_via(
    storage: &Storage,
    group_id: &str,
    control: &GroupCallControlV1,
) -> OmniResult<()> {
    let payload = GroupAppPayloadV1 {
        message_id: random_id("gc-"),
        kind: GroupAppKindV1::Control,
        body: serde_json::to_string(control).map_err(|e| OmniError::Internal(e.to_string()))?,
        authored_at_unix_ms: current_unix_ms(),
        signature_b64: None,
        reply_to: None,
        mentions: Vec::new(),
    };
    send_app_payload(storage, group_id, &payload)
}

pub(crate) fn own_commit_bookkeeping(storage: &Storage, group_id: &str, commit: &[u8], change: &OwnChange) -> OmniResult<()> {
    let gid = group_id_bytes(group_id)?;
    let view = with_engine(storage, |engine| engine.view(&gid))?;
    let now = current_unix_ms();
    storage.record_group_own_change(
        group_id,
        view.epoch,
        &serde_json::to_string(change).map_err(|e| OmniError::Internal(e.to_string()))?,
    )?;
    storage.enqueue_group_feed(group_id, commit, now)?;
    apply_view(storage, group_id, &view, now)?;
    Ok(())
}

/// Refresh the group row + member projection from an MLS view; write system lines for who came and
/// who went. Returns whether we are still in the group.
pub(crate) fn apply_view(storage: &Storage, group_id: &str, view: &GroupView, now: i64) -> OmniResult<bool> {
    let before = storage.list_group_members(group_id)?;
    // A member that said it left stays «left» until a Commit takes it out: a Commit about someone else
    // made it a member again, and its lines counted (code audit 2026-11-09, OMNI-02).
    let said_it_left = |member_pub: &str| before.iter().any(|b| b.member_pub == member_pub && b.status == "left");
    let members: Vec<GroupMemberRecord> = view
        .members
        .iter()
        .map(|m| GroupMemberRecord {
            node_hint_hex: node_hex_of(&m.card.member_pub),
            member_pub: m.card.member_pub.clone(),
            // Only an account the card's device chain proves: everything that trusts a member's
            // account reads it from here (approvals, authorship, our own devices).
            account_id: card_with_proven_account(&m.card).account_id,
            display_name: m.card.display_name.clone(),
            role: if m.is_admin { "admin".into() } else { "member".into() },
            status: if said_it_left(&m.card.member_pub) { "left".into() } else { "member".into() },
        })
        .collect();
    storage.replace_group_members(group_id, &members)?;
    // Someone left every group we share: their card goes with them.
    let _ = storage.forget_group_member_profiles_outside_groups();
    if let Some(group) = storage.get_group(group_id)? {
        if !before.is_empty() {
            // The lines are about people, the members are devices: another device of someone already
            // here, or one of theirs going while another stays, says nothing (stand 2026-10-13: a
            // restored phone made «Алиса в группе» twice).
            let came = members.iter().filter(|m| !before.iter().any(|b| b.member_pub == m.member_pub));
            let mut announced: Vec<&str> = Vec::new();
            for m in came {
                if before.iter().any(|b| b.account_id == m.account_id) || announced.contains(&m.account_id.as_str()) {
                    continue;
                }
                announced.push(&m.account_id);
                insert_system_line(storage, group_id, &format!("{} в группе", m.display_name), now)?;
            }
            let mut gone: Vec<&str> = Vec::new();
            for b in before.iter().filter(|b| !members.iter().any(|m| m.account_id == b.account_id)) {
                if gone.contains(&b.account_id.as_str()) {
                    continue;
                }
                gone.push(&b.account_id);
                // Without «(а)»: the line does not know the member's gender, and the brackets read as a
                // form to fill (owner, 2026-10-13: UI/UX pass).
                let line = if b.status == "left" {
                    format!("{} покидает группу", b.display_name)
                } else {
                    format!("{} исключили из группы", b.display_name)
                };
                insert_system_line(storage, group_id, &line, now)?;
            }
            // A removed member must lose the running call too: drop them and rotate its secret
            // (this view is already the new epoch, so the Rekey we may queue is unreadable to them).
            let removed: Vec<String> = before
                .iter()
                .filter(|b| !members.iter().any(|m| m.member_pub == b.member_pub))
                .map(|b| b.member_pub.clone())
                .collect();
            let db_path = storage.db_path().to_string_lossy().to_string();
            if let Some(rekey) = crate::app_group_calls::on_group_members_removed(&db_path, group_id, &removed)? {
                // Not said: owed again, or we alone would hold the new secret.
                if let Err(error) = publish_group_call_control_via(storage, group_id, &rekey) {
                    crate::app_group_calls::owe_group_call_rekey(&db_path, group_id, rekey);
                    return Err(error);
                }
            }
            if group.name != view.name {
                insert_system_line(storage, group_id, &format!("Новое название: «{}»", view.name), now)?;
            }
            let had_avatar = storage.get_group_avatar_hash(group_id)?;
            let avatar_changed = sync_group_avatar(storage, group_id, view.avatar.as_deref())?;
            if avatar_changed {
                let line = if view.avatar.is_none() {
                    "Аватарка группы убрана"
                } else if had_avatar.is_none() {
                    "У группы появилась аватарка"
                } else {
                    "Новая аватарка группы"
                };
                insert_system_line(storage, group_id, line, now)?;
            }
            // Role changes are as visible as joins: everyone should know who can remove people now.
            for m in &members {
                let Some(b) = before.iter().find(|b| b.member_pub == m.member_pub) else { continue };
                if b.role != "admin" && m.role == "admin" {
                    insert_system_line(storage, group_id, &format!("{} — теперь админ", m.display_name), now)?;
                } else if b.role == "admin" && m.role != "admin" {
                    insert_system_line(storage, group_id, &format!("{} больше не админ", m.display_name), now)?;
                }
            }
        }
        if before.is_empty() {
            // First view of the group here (created it, or joined from a Welcome): its picture.
            sync_group_avatar(storage, group_id, view.avatar.as_deref())?;
        }
        let my_status = if view.active { "member" } else { "removed" };
        if !view.active {
            crate::app_group_calls::end_group_call_here(&storage.db_path().to_string_lossy(), group_id);
        }
        if !view.active && group.my_status == "member" {
            insert_system_line(storage, group_id, "Вас удалили из группы", now)?;
        }
        storage.upsert_group(&GroupRecord {
            name: view.name.clone(),
            my_status: my_status.into(),
            updated_at_unix_ms: now,
            ..group
        })?;
    }
    crate::record_group_changed_event(&storage.db_path().to_string_lossy(), group_id);
    Ok(view.active)
}

/// Keep the picture from the group's meta. Returns whether it changed. A picture that is not valid
/// base64 is dropped (the meta is the admins', not ours to trust blindly; showing nothing is safe).
fn sync_group_avatar(storage: &Storage, group_id: &str, avatar_b64: Option<&str>) -> OmniResult<bool> {
    let bytes = avatar_b64.and_then(|b64| B64.decode(b64).ok());
    let wanted = bytes.as_deref().map(|b| {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(b))
    });
    if storage.get_group_avatar_hash(group_id)? == wanted {
        return Ok(false);
    }
    storage.set_group_avatar(group_id, bytes.as_deref())?;
    Ok(true)
}

pub(crate) fn insert_system_line(storage: &Storage, group_id: &str, text: &str, now: i64) -> OmniResult<()> {
    let id = random_id("gs-");
    let dialog_key = group_dialog_key(group_id);
    storage.insert_incoming_message(&id, &dialog_key, text, now)?;
    storage.insert_group_message_meta(
        group_id,
        &GroupMessageMeta { message_id: id, sender_pub: None, sender_name: String::new(), kind: "system".into() },
    )
}

fn accept_invite_via(
    storage: &Storage,
    transport: &crate::TransportManager,
    invite: &GroupInviteRecord,
) -> OmniResult<()> {
    let key_package = own_key_package(storage)?;
    let accept = GroupInviteAcceptV2 {
        version: GROUP_WIRE_VERSION_V2,
        group_id: invite.group_id.clone(),
        key_package_b64: B64.encode(key_package),
    };
    send_control_1to1(storage, transport, &invite.inviter_dialog_key, GroupControlKind::InviteAccept, &accept)?;
    storage.upsert_group_invite(&GroupInviteRecord { status: "accepted".into(), ..invite.clone() })
}

/// Ingest one MLS message from a group feed (live gossip and store-and-forward alike).
pub(crate) fn ingest_group_feed_item(
    storage: &Storage,
    transport: &crate::TransportManager,
    item_json: &str,
) -> OmniResult<()> {
    let item: GroupFeedItemV2 =
        serde_json::from_str(item_json).map_err(|e| OmniError::InvalidInput(format!("group feed item: {e}")))?;
    omni_protocol::validate_group_feed_item_v2(&item)?;
    let mls = B64
        .decode(&item.mls_b64)
        .map_err(|e| OmniError::InvalidInput(format!("group mls b64: {e}")))?;
    let group_id = item.group_id;
    match storage.get_group(&group_id)? {
        Some(group) if group.my_status == "member" => {}
        // Not in it (yet): if we accepted an invitation, the Welcome may still be on its way — keep it.
        _ => {
            if storage.get_group_invite(&group_id)?.is_some_and(|i| i.status == "accepted")
                && !storage.push_group_pending_inbound(&group_id, 0, &mls, current_unix_ms())?
            {
                crate::omni_app_trace_log("group/pending", &format!("group={group_id} not kept: it holds as much as it may before its Welcome"));
            }
            return Ok(());
        }
    }
    receive_mls(storage, transport, &group_id, &mls, false)
}

/// A line's time as kept and shown: its author's, but not more than an hour past ours — a line dated
/// far ahead stayed the newest of the chat for good (code audit 2026-11-09, OMNI-03). Its signed copy
/// keeps the author's time as signed.
pub(crate) fn shown_line_time(authored_at_unix_ms: i64, now_unix_ms: i64) -> i64 {
    authored_at_unix_ms.min(now_unix_ms.saturating_add(omni_storage::PIN_CLOCK_AHEAD_MS))
}

/// What [`receive_mls`] took in its transaction: a Commit, done there, or anything else, handled after.
enum Received {
    Gone,
    Commit { still_in: bool, reissue: Option<u64> },
    Other(Inbound),
}

pub(crate) fn receive_mls(
    storage: &Storage,
    transport: &crate::TransportManager,
    group_id: &str,
    mls: &[u8],
    pending: bool,
) -> OmniResult<()> {
    let gid = group_id_bytes(group_id)?;
    let now = current_unix_ms();
    let db_path = storage.db_path().to_string_lossy().to_string();
    // A Commit and what it changes here — the view, our messages of a rolled-back epoch sent again,
    // our own change made again — are one transaction with the MLS step that merged it: a failure
    // between them left the group's crypto state and its members here apart (blind audit
    // 2026-11-12, pass 17). Rolled back, the Commit comes again.
    let received = in_mls_transaction(storage, "group_receive", |storage| {
        // A message that waited for its epoch leaves the queue in the step that handles it: rolled
        // back, it is there again. One already gone was handled on the way (a Commit opened it).
        if pending && !storage.delete_group_pending_inbound(group_id, mls)? {
            return Ok(Received::Gone);
        }
        let inbound = with_engine(storage, |engine| engine.receive(&gid, mls))?;
        let Inbound::Commit { outcome, view, put_back } = inbound else {
            return Ok(Received::Other(inbound));
        };
        for member_pub in &put_back {
            crate::app_group_branches::note_put_back(storage, group_id, member_pub);
        }
        match &outcome {
            CommitOutcome::Rejected(reason) => {
                crate::omni_app_trace_log("group/commit", &format!("group={group_id} rejected: {reason}"));
                return Ok(Received::Commit { still_in: false, reissue: None });
            }
            CommitOutcome::LostIgnored => return Ok(Received::Commit { still_in: false, reissue: None }),
            CommitOutcome::Merged | CommitOutcome::ReplacedLoser { .. } => {}
        }
        let still_in = apply_view(storage, group_id, &view, now)?;
        let mut reissue = None;
        if let CommitOutcome::ReplacedLoser { our_commit_lost } = outcome {
            // Our messages of the rolled-back epoch did not reach the winning branch: re-send.
            for json in storage.list_group_outbox_since(group_id, view.epoch)? {
                if let Ok(payload) = serde_json::from_str::<GroupAppPayloadV1>(&json) {
                    let _ = send_app_payload(storage, group_id, &payload);
                }
            }
            if our_commit_lost {
                reissue = Some(view.epoch);
            }
        }
        Ok(Received::Commit { still_in, reissue })
    })?;
    let inbound = match received {
        Received::Gone => return Ok(()),
        Received::Commit { still_in, reissue } => {
            // Our lost change is made again once the Commit stands: making it may send 1:1 frames
            // (invitations), which never go out of an open transaction (pass 18). Kept until made.
            if let Some(epoch) = reissue {
                reissue_own_change(storage, transport, group_id, epoch)?;
            }
            if still_in {
                // Messages that were ahead of us may open now — each its own step, after the Commit stands.
                open_pending_inbound(storage, transport, group_id)?;
            }
            return Ok(());
        }
        Received::Other(inbound) => inbound,
    };
    match inbound {
        Inbound::Application { sender, payload, .. } => {
            let Ok(payload) = serde_json::from_slice::<GroupAppPayloadV1>(&payload) else {
                return Ok(());
            };
            omni_protocol::validate_group_app_payload_v1(&payload)?;
            let group = storage
                .get_group(group_id)?
                .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
            if sender.member_pub == group.my_member_pub {
                return Ok(()); // our own message echoed back
            }
            // What the handlers below trust the sender's account for, they get only when proven.
            let sender = card_with_proven_account(&sender);
            // A device its account revoked: nothing it says counts, even before a Commit takes it out
            // (one that ignores its order to wipe still holds the epoch's keys).
            if card_device_revoked(storage, &sender) {
                crate::omni_app_trace_log("group/revoked", &format!("group={group_id} a line from a revoked device dropped"));
                return Ok(());
            }
            // Only a member in the group now is heard (code audit 2026-11-09, OMNI-02): one that said
            // it left holds the epoch's keys until an admin's Commit takes it out, and one taken out
            // still opens a line of an epoch before its removal that comes late.
            let Some(sender_record) = storage.list_group_members(group_id)?.into_iter().find(|m| m.member_pub == sender.member_pub) else {
                crate::omni_app_trace_log("group/ingest", &format!("group={group_id} a line from one not in the group dropped"));
                return Ok(());
            };
            if sender_record.status == "left" {
                crate::omni_app_trace_log("group/ingest", &format!("group={group_id} a line from a member that left dropped"));
                return Ok(());
            }
            // It opened in our epoch: the sender is on our branch, whatever it asked before.
            let _ = storage.drop_group_readd_request(group_id, &sender.member_pub);
            // Heard in the group: its Welcome, if we kept one for it, arrived.
            let _ = storage.drop_group_invitee_welcomes_of(group_id, &sender.account_id);
            if !crate::app_group_history::keep_member_line(storage, group_id, &sender, &payload) {
                return Ok(());
            }
            match payload.kind {
                GroupAppKindV1::History => {
                    crate::app_group_history::receive_history(storage, group_id, &group, &payload)?;
                    crate::record_group_changed_event(&db_path, group_id);
                }
                GroupAppKindV1::MessageDelete => {
                    crate::app_message_actions::receive_group_delete(storage, &group.dialog_key, &sender, &payload)?;
                    crate::record_group_changed_event(&db_path, group_id);
                }
                GroupAppKindV1::MessagePin => {
                    crate::app_message_actions::receive_group_pin(storage, &group.dialog_key, &payload)?;
                    crate::record_group_changed_event(&db_path, group_id);
                }
                GroupAppKindV1::Text if storage.message_is_deleted(&payload.message_id)? => {}
                // Here already (a resend), or the id is another chat's: nothing of it is stored again.
                GroupAppKindV1::Text if storage.message_exists(&payload.message_id)? => {
                    let _ = crate::app_message_actions::message_id_taken_elsewhere(storage, &payload.message_id, &group.dialog_key);
                }
                GroupAppKindV1::Text => {
                    // The line and what belongs to it (the reply, the mentions, its author) as one: written
                    // apart, a failure between them left the line without them, and the line sent again
                    // was taken for one already here (blind audit 2026-11-13, pass 18).
                    let (record, deleted) = storage.with_immediate_transaction("group text line", |storage| {
                        let record = storage.insert_incoming_message(
                            &payload.message_id,
                            &group.dialog_key,
                            &payload.body,
                            shown_line_time(payload.authored_at_unix_ms, now),
                        )?;
                        storage.remember_received_message_reply(&payload.message_id, payload.reply_to.as_deref())?;
                        storage.remember_message_mentions(&payload.message_id, &payload.mentions)?;
                        storage.insert_group_message_meta(
                            group_id,
                            &GroupMessageMeta {
                                message_id: payload.message_id.clone(),
                                sender_pub: Some(sender.member_pub.clone()),
                                sender_name: sender.display_name.clone(),
                                kind: "text".into(),
                            },
                        )?;
                        let deleted = crate::app_message_actions::apply_pending_message_deletes(storage, &payload.message_id)?;
                        Ok((record, deleted))
                    })?;
                    if deleted {
                        crate::record_group_changed_event(&db_path, group_id);
                    } else {
                        crate::record_inbound_message_event(&db_path, &group.dialog_key, &record.id, &payload.body);
                    }
                }
                GroupAppKindV1::Reaction => {
                    crate::app_message_reactions::receive_group_reaction(storage, &group.dialog_key, &sender, &payload)?;
                    crate::record_group_changed_event(&db_path, group_id);
                }
                GroupAppKindV1::Attachment => {
                    crate::app_group_attachments::receive_offer(storage, group_id, &group, &sender, &payload)?;
                }
                GroupAppKindV1::Control => {
                    if let Ok(control) = serde_json::from_str::<GroupCallControlV1>(&payload.body) {
                        let emits = crate::app_group_calls::handle_group_call_control(
                            storage,
                            group_id,
                            &sender.member_pub,
                            &control,
                        )?;
                        for reply in &emits {
                            publish_group_call_control_via(storage, group_id, reply)?;
                        }
                    }
                }
                GroupAppKindV1::Dissolve => {
                    let by_admin = sender_record.role == "admin";
                    if !by_admin {
                        crate::omni_app_trace_log("group/ingest", &format!("group={group_id} dissolve from a non-admin ignored"));
                        return Ok(());
                    }
                    // The group's MLS state goes with its row marked dissolved, as one: apart, a failure
                    // between them left a group shown as ours with no keys to open it (pass 18).
                    in_mls_transaction(storage, "group dissolved", |storage| {
                        with_engine(storage, |engine| engine.delete_group(&gid))?;
                        storage.upsert_group(&GroupRecord {
                            my_status: "dissolved".into(),
                            updated_at_unix_ms: now,
                            ..group
                        })?;
                        insert_system_line(storage, group_id, &format!("Группа удалена ({})", sender.display_name), now)
                    })?;
                    crate::app_group_calls::end_group_call_here(&db_path, group_id);
                    crate::record_group_changed_event(&db_path, group_id);
                }
                GroupAppKindV1::Profile => {
                    if crate::app_group_member_profiles::receive_profile(storage, &sender, &payload.body)? {
                        crate::record_group_changed_event(&db_path, group_id);
                    }
                }
                GroupAppKindV1::ProfileAsk => {
                    crate::app_group_member_profiles::answer_profile_ask(storage, group_id)?;
                }
                GroupAppKindV1::WorkspaceRefs => {
                    crate::app_workspace::receive_refs(storage, group_id, &sender, &payload.body, now)?;
                }
                GroupAppKindV1::WorkspaceRule => {
                    crate::app_workspace::receive_rule(storage, group_id, &sender, &payload.body)?;
                }
                GroupAppKindV1::WorkspaceApprove => {
                    crate::app_workspace::receive_approval(storage, group_id, &sender, &payload.body, now)?;
                }
                GroupAppKindV1::WorkspaceFiles => {
                    crate::app_workspace::custody::receive(storage, group_id, &sender, &payload.body)?;
                }
                GroupAppKindV1::WorkspaceAgents => {
                    crate::app_workspace::agents::receive(storage, group_id, &sender, &payload.body)?;
                }
                GroupAppKindV1::Leave => {
                    storage.set_group_member_status(group_id, &sender.member_pub, "left")?;
                    crate::record_group_changed_event(&db_path, group_id);
                    // An admin turns the farewell into a Commit (concurrent admins: the rank rule).
                    let pubs = vec![sender.member_pub.clone()];
                    in_mls_transaction(storage, "group_own_commit", |storage| {
                        if let Ok(commit) = with_engine(storage, |engine| engine.remove_members(&gid, &pubs)) {
                            own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::Remove { member_pubs: pubs })?;
                        }
                        Ok(())
                    })?;
                }
            }
        }
        Inbound::Commit { .. } => {}
        Inbound::Future { epoch } => {
            // Not kept (the queue is full of nearer ones): not handled either — it comes again, by
            // then perhaps opened in its turn (pass 20). Taken as handled, it was lost.
            if !storage.push_group_pending_inbound(group_id, epoch, mls, now)? {
                return Err(crate::not_yet(format!("group={group_id} epoch={epoch} not kept: the group holds as much as it may")));
            }
        }
        Inbound::ForkSeen { epoch, ours_wins } => {
            crate::app_group_branches::on_fork_seen(storage, group_id, epoch, ours_wins, now)?;
        }
        Inbound::RejoinRequest { card, key_package } => {
            crate::app_group_branches::on_rejoin_request(storage, group_id, &card, &key_package, now)?;
        }
        Inbound::Welcome(welcome) => {
            crate::app_group_branches::on_welcome(storage, transport, group_id, &welcome, now)?;
        }
        Inbound::Ignored(reason) => {
            crate::omni_app_trace_log("group/ingest", &format!("group={group_id} ignored: {reason}"));
        }
        // Not kept here, so not handled: the frame is to come again (the mesh's RetryAWhile, and after
        // a restart), once we are nearer its epoch. Taken as handled, an honest message of a member far ahead of us after a
        // long time offline was lost for good (blind audit 2026-11-13, pass 20).
        Inbound::TooFarAhead { epoch, current } => {
            return Err(crate::not_yet(format!("group={group_id} epoch {epoch} is too far ahead of ours ({current})")));
        }
    }
    Ok(())
}

/// Our Commit for `epoch_after` lost to a concurrent one: do the change again on the winning branch.
fn reissue_own_change(
    storage: &Storage,
    transport: &crate::TransportManager,
    group_id: &str,
    epoch_after: u64,
) -> OmniResult<()> {
    let Some(json) = storage.get_group_own_change(group_id, epoch_after)? else {
        return Ok(());
    };
    reissue_kept_change(storage, transport, group_id, epoch_after, &json)
}

/// Make our change kept for `epoch_after` again, and let it go only once it is made. Taken off first,
/// a failure on the way (no session for an invitee, pass 17) or a crash (pass 19) lost it with
/// nothing left to try again from. An MLS change goes in the transaction of the Commit that makes it
/// again; an invitation (sent 1:1, never out of a transaction) goes once sent. One that does not
/// read goes.
pub(crate) fn reissue_kept_change(
    storage: &Storage,
    transport: &crate::TransportManager,
    group_id: &str,
    epoch_after: u64,
    json: &str,
) -> OmniResult<()> {
    let Ok(change) = serde_json::from_str::<OwnChange>(json) else {
        return storage.drop_group_own_change(group_id, epoch_after, json);
    };
    if matches!(change, OwnChange::Add { .. }) {
        reissue_change(storage, transport, group_id, change)?;
        return storage.drop_group_own_change(group_id, epoch_after, json);
    }
    in_mls_transaction(storage, "own change again", |storage| {
        storage.drop_group_own_change(group_id, epoch_after, json)?;
        reissue_change(storage, transport, group_id, change)
    })
}

/// Make one of our own changes again, on the branch we are on now.
pub(crate) fn reissue_change(
    storage: &Storage,
    transport: &crate::TransportManager,
    group_id: &str,
    change: OwnChange,
) -> OmniResult<()> {
    let gid = group_id_bytes(group_id)?;
    match change {
        OwnChange::PutBack { .. } | OwnChange::RefreshCard => {}
        OwnChange::Add { dialog_keys } => {
            // The Welcome we sent is for a dead branch: ask the invitees for a fresh KeyPackage.
            let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
            let (_, me) = local_mls_identity(storage)?;
            for dialog_key in dialog_keys {
                let invite = GroupInviteV2 {
                    version: GROUP_WIRE_VERSION_V2,
                    group_id: group_id.to_string(),
                    name: group.name.clone(),
                    inviter_name: me.display_name.clone(),
                    renew: true,
                    authored_at_unix_ms: current_unix_ms(),
                };
                send_control_1to1(storage, transport, &dialog_key, GroupControlKind::Invite, &invite)?;
                storage.record_group_sent_invite(group_id, &dialog_key, current_unix_ms())?;
            }
        }
        OwnChange::Remove { member_pubs } => {
            let still: Vec<String> = with_engine(storage, |engine| engine.view(&gid))?
                .members
                .iter()
                .filter(|m| member_pubs.contains(&m.card.member_pub))
                .map(|m| m.card.member_pub.clone())
                .collect();
            if !still.is_empty() {
                in_mls_transaction(storage, "group_own_commit", |storage| {
                    let commit = with_engine(storage, |engine| engine.remove_members(&gid, &still))?;
                    own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::Remove { member_pubs: still })
                })?;
            }
        }
        OwnChange::Rename { name } => {
            let new_name = name.clone();
            in_mls_transaction(storage, "group_own_commit", |storage| {
                let commit = with_engine(storage, |engine| engine.update_meta(&gid, |m| m.name = new_name))?;
                own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::Rename { name })
            })?;
        }
        OwnChange::Admins { admins } => {
            let wanted = admins.clone();
            in_mls_transaction(storage, "group_own_commit", |storage| {
                let commit = with_engine(storage, |engine| engine.update_meta(&gid, |m| m.admins = wanted))?;
                own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::Admins { admins })
            })?;
        }
        OwnChange::Avatar { avatar } => {
            let wanted = avatar.clone();
            in_mls_transaction(storage, "group_own_commit", |storage| {
                let commit = with_engine(storage, |engine| engine.update_meta(&gid, |m| m.avatar = wanted))?;
                own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::Avatar { avatar })
            })?;
        }
        OwnChange::NewcomerHistory { on } => {
            in_mls_transaction(storage, "group_own_commit", |storage| {
                let commit = with_engine(storage, |engine| engine.update_meta(&gid, |m| m.newcomer_history = on))?;
                own_commit_bookkeeping(storage, group_id, &commit, &OwnChange::NewcomerHistory { on })
            })?;
        }
    }
    Ok(())
}

// --- consent over 1:1 --------------------------------------------------------------------------

pub(crate) fn handle_incoming_group_control(
    storage: &Storage,
    transport: &crate::TransportManager,
    receive_context: &crate::ReceivedChatContext,
    control: &omni_types::GroupControlV1,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let decode = |what: &str| OmniError::InvalidInput(format!("group {what} decode"));
    match GroupControlKind::from_wire(&control.control) {
        Some(GroupControlKind::Invite) => {
            let invite: GroupInviteV2 = serde_json::from_str(&control.payload_json).map_err(|_| decode("invite"))?;
            omni_protocol::validate_group_invite_v2(&invite)?;
            if storage.get_group(&invite.group_id)?.is_some_and(|g| g.my_status == "member") {
                return Ok(()); // already in
            }
            let previous = storage.get_group_invite(&invite.group_id)?;
            let record = GroupInviteRecord {
                group_id: invite.group_id.clone(),
                name: invite.name.clone(),
                // Who invited is the contact the session is with, not the name the invitation gives:
                // a contact could pass its invitation off as another's (blind audit 2026-11-13, pass 20).
                inviter_name: Some(contact_name_for_dialog(storage, &receive_context.dialog_key))
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| invite.inviter_name.clone()),
                inviter_dialog_key: receive_context.dialog_key.clone(),
                status: "pending".into(),
                received_at_unix_ms: current_unix_ms(),
            };
            // A renewal of an invitation we already accepted: answer without asking again.
            if invite.renew && previous.as_ref().is_some_and(|p| p.status == "accepted") {
                return accept_invite_via(storage, transport, &record);
            }
            storage.upsert_group_invite(&record)?;
            crate::record_group_changed_event(&db_path, &invite.group_id);
            Ok(())
        }
        Some(GroupControlKind::InviteAccept) => {
            let accept: GroupInviteAcceptV2 =
                serde_json::from_str(&control.payload_json).map_err(|_| decode("accept"))?;
            match add_invitee(storage, &receive_context.dialog_key, &accept)? {
                Some(welcome) => send_control_1to1(
                    storage,
                    transport,
                    &receive_context.dialog_key,
                    GroupControlKind::Welcome,
                    &welcome,
                ),
                None => Ok(()),
            }
        }
        Some(GroupControlKind::DeviceJoin) => {
            // A device of a contact's account asks in (a phone restored from the phrase, say).
            let request: GroupInviteAcceptV2 =
                serde_json::from_str(&control.payload_json).map_err(|_| decode("device join"))?;
            let Some(account_id) = dialog_account(storage, &receive_context.dialog_key)? else {
                return Ok(());
            };
            let known = crate::app_group_rejoin::account_device_pubs(storage, &account_id)?;
            match crate::app_group_rejoin::add_account_device(
                storage,
                &receive_context.dialog_key,
                &account_id,
                &known,
                &request,
            )? {
                Some(welcome) => send_control_1to1(
                    storage,
                    transport,
                    &receive_context.dialog_key,
                    GroupControlKind::Welcome,
                    &welcome,
                ),
                None => Ok(()),
            }
        }
        Some(GroupControlKind::DeviceOffer) => {
            // Our other device, a member, says we are not in one of its groups: ask it to add us.
            let offer: GroupInviteV2 = serde_json::from_str(&control.payload_json).map_err(|_| decode("device offer"))?;
            omni_protocol::validate_group_invite_v2(&offer)?;
            if !own_device_dialog(storage, &receive_context.dialog_key)? {
                return Ok(());
            }
            crate::app_group_rejoin::answer_device_offer(storage, transport, &receive_context.dialog_key, &offer)
        }
        Some(GroupControlKind::Welcome) => {
            let welcome: GroupWelcomeV2 = serde_json::from_str(&control.payload_json).map_err(|_| decode("welcome"))?;
            join_from_welcome(storage, transport, &receive_context.dialog_key, &welcome)
        }
        Some(GroupControlKind::InviteDecline) => {
            let decline: GroupInviteDeclineV2 =
                serde_json::from_str(&control.payload_json).map_err(|_| decode("decline"))?;
            // One message, two directions, both "this invitation is off" — told apart by who holds
            // what, and only ever for the 1:1 peer it concerns.
            if storage.take_group_sent_invite(&decline.group_id, &receive_context.dialog_key)? {
                // The invitee said no.
                let name = contact_name_for_dialog(storage, &receive_context.dialog_key);
                insert_system_line(storage, &decline.group_id, &format!("Приглашение отклонено: {name}"), current_unix_ms())?;
                crate::record_group_changed_event(&db_path, &decline.group_id);
            } else if let Some(invite) = storage.get_group_invite(&decline.group_id)? {
                // The inviter withdrew it — only theirs to withdraw, and not once we are in.
                if invite.inviter_dialog_key == receive_context.dialog_key
                    && (invite.status == "pending" || invite.status == "accepted")
                    && storage.get_group(&decline.group_id)?.is_none_or(|g| g.my_status != "member")
                {
                    storage.upsert_group_invite(&GroupInviteRecord { status: "withdrawn".into(), ..invite })?;
                    crate::record_group_changed_event(&db_path, &decline.group_id);
                }
            }
            Ok(())
        }
        None => Ok(()),
    }
}

/// How the group thread names a contact: our petname, else their public name.
fn contact_name_for_dialog(storage: &Storage, dialog_key: &str) -> String {
    storage
        .resolve_contact_for_dialog(dialog_key)
        .ok()
        .flatten()
        .map(|c| {
            if !c.petname.trim().is_empty() {
                c.petname
            } else {
                c.public_display_name.unwrap_or_default()
            }
        })
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "контакт".to_string())
}

/// An invitee we invited accepted with `accept`: add them (a Commit to the group) and return the
/// Welcome to send back over 1:1. `None` — not someone we invited for this group (or we are no longer
/// in it): the accept is ignored, so nobody can talk their way into a group by knowing its id.
pub(crate) fn add_invitee(
    storage: &Storage,
    invitee_dialog_key: &str,
    accept: &GroupInviteAcceptV2,
) -> OmniResult<Option<GroupWelcomeV2>> {
    omni_protocol::validate_group_invite_accept_v2(accept)?;
    if storage.get_group(&accept.group_id)?.is_none_or(|g| g.my_status != "member") {
        return Ok(None);
    }
    // Added already, its Welcome kept: the invitee asks again because it did not arrive — it goes
    // again (maintainers' decision 2026-11-11). Past the Commit the invitation is spent; without this, a
    // Welcome lost on the way left the invitee in the tree and out of the group for good.
    if !storage.list_group_sent_invites(&accept.group_id)?.iter().any(|(dialog, _)| dialog == invitee_dialog_key) {
        if let Some((_, json)) = kept_welcomes_for(storage, invitee_dialog_key, Some(&accept.group_id))?.into_iter().next() {
            crate::omni_app_trace_log("group/add", &format!("group={} the kept Welcome goes again to {invitee_dialog_key}", accept.group_id));
            return Ok(serde_json::from_str(&json).ok());
        }
    }
    let gid = group_id_bytes(&accept.group_id)?;
    let kp = B64
        .decode(&accept.key_package_b64)
        .map_err(|_| OmniError::InvalidInput("group key package b64".into()))?;
    // The KeyPackage is for the contact we invited: its account (which its device chain proves) is the
    // account of the chat the accept came over.
    let card = with_engine(storage, |engine| engine.key_package_card(&kp))?;
    if let Some(account) = storage.resolve_contact_for_dialog(invitee_dialog_key)?.and_then(|c| c.remote_account_id) {
        if card.account_id != account.0 {
            crate::omni_app_trace_log("group/add", &format!("group={} accept over {invitee_dialog_key} names another account", accept.group_id));
            return Ok(None);
        }
    }
    // Spent only now, by an accept we can take: spent first, an accept whose KeyPackage did not read
    // (a client's fault, a transient engine error) lost the invitation for good — the next, good one
    // found none (blind audit 2026-11-11, G2).
    // The invitation is spent, the member added and its Welcome kept, as one: spent apart, a crash
    // before the add lost the invitation, and the accept sent again found neither it nor a Welcome
    // (blind audit 2026-11-13, pass 19). Refused by the engine, nothing changed and the invitation
    // stays for another accept (H5).
    let added = in_mls_transaction(storage, "group_own_commit", |storage| {
        if !storage.take_group_sent_invite(&accept.group_id, invitee_dialog_key)? {
            return Ok(None);
        }
        let out = with_engine(storage, |engine| engine.add_members(&gid, &[kp]))?;
        own_commit_bookkeeping(
            storage,
            &accept.group_id,
            &out.commit,
            &OwnChange::Add { dialog_keys: vec![invitee_dialog_key.to_string()] },
        )?;
        let welcome = GroupWelcomeV2 {
            version: GROUP_WIRE_VERSION_V2,
            group_id: accept.group_id.clone(),
            welcome_b64: B64.encode(&out.welcome),
        };
        let json = serde_json::to_string(&welcome).map_err(|e| OmniError::Internal(e.to_string()))?;
        storage.put_group_invitee_welcome(&accept.group_id, invitee_dialog_key, &card.account_id, &json, current_unix_ms())?;
        Ok(Some(welcome))
    })?;
    let Some(welcome) = added else {
        return Ok(None);
    };
    // The newcomer gets the recent history in the epoch it joins (best effort: the add stands).
    let newcomer_history = with_engine(storage, |engine| engine.view(&gid)).map(|view| view.newcomer_history).unwrap_or(true);
    if let Err(error) = crate::app_group_history::send_newcomer_history(storage, &accept.group_id, newcomer_history) {
        crate::omni_app_trace_log("group/history", &format!("group={} history not sent: {error}", accept.group_id));
    }
    Ok(Some(welcome))
}

/// How long a Welcome is kept for an invitee not heard in the group yet.
const PENDING_WELCOME_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn kept_welcomes_for(storage: &Storage, dialog_key: &str, group_id: Option<&str>) -> OmniResult<Vec<(String, String)>> {
    storage.group_invitee_welcomes_for(dialog_key, group_id, current_unix_ms() - PENDING_WELCOME_KEEP_MS)
}

/// The session with `dialog_key` came up: the Welcomes kept for it go again — the one sent may have
/// gone where nobody heard it. An invitee in the group already ignores one (it joins only from an
/// invitation it accepted, `join_from_welcome`).
pub(crate) fn resend_kept_welcomes(storage: &Storage, transport: &crate::TransportManager, dialog_key: &str) {
    let Ok(kept) = kept_welcomes_for(storage, dialog_key, None) else {
        return;
    };
    for (group_id, json) in kept {
        let Ok(welcome) = serde_json::from_str::<GroupWelcomeV2>(&json) else {
            continue;
        };
        let sent = send_control_1to1(storage, transport, dialog_key, GroupControlKind::Welcome, &welcome);
        crate::omni_app_trace_log("group/add", &format!("group={group_id} kept Welcome to {dialog_key} again: {}", sent.is_ok()));
    }
}

/// Join a group from its MLS Welcome — only for an invitation we accepted, and only from the one who
/// invited us (`from_dialog_key`): before, any approved contact that knew the group id could send one
/// (V-15).
pub(crate) fn join_from_welcome(
    storage: &Storage,
    transport: &crate::TransportManager,
    from_dialog_key: &str,
    welcome: &GroupWelcomeV2,
) -> OmniResult<()> {
    omni_protocol::validate_group_welcome_v2(welcome)?;
    let Some(invite) = storage.get_group_invite(&welcome.group_id)? else {
        return Ok(());
    };
    if invite.status != "accepted" || invite.inviter_dialog_key != from_dialog_key {
        return Ok(());
    }
    let bytes = B64
        .decode(&welcome.welcome_b64)
        .map_err(|_| OmniError::InvalidInput("group welcome b64".into()))?;
    let gid = group_id_bytes(&welcome.group_id)?;
    // A renewed Welcome replaces a group joined on a branch that lost — but what we hold goes only
    // once the new Welcome opened, for this group: a junk one leaves the group as it was.
    // Joined in MLS and a member here, as one: apart, a failure between them left MLS state of a
    // group we do not show, and the Welcome again found it joined (blind audit 2026-11-13, pass 18).
    let now = current_unix_ms();
    in_mls_transaction(storage, "group joined", |storage| {
        with_engine(storage, |engine| engine.join_group_from_welcome(&gid, &bytes))?;
        let view = with_engine(storage, |engine| engine.view(&gid))?;
        let (_, me) = local_mls_identity(storage)?;
        storage.upsert_group(&GroupRecord {
            group_id: welcome.group_id.clone(),
            name: view.name.clone(),
            my_member_pub: me.member_pub,
            my_status: "member".into(),
            dialog_key: group_dialog_key(&welcome.group_id),
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        })?;
        apply_view(storage, &welcome.group_id, &view, now)?;
        insert_system_line(storage, &welcome.group_id, "Вы вступили в группу", now)
    })?;
    crate::app_group_member_profiles::introduce_after_join(storage, &welcome.group_id);
    open_pending_inbound(storage, transport, &welcome.group_id)
}

/// Open the messages that waited for the epoch we are in now. Each leaves the queue in the step
/// that handles it (`receive_mls`): taken all at once before, a crash on the way lost them (pass 18).
/// One that fails for a passing reason (the database, the engine) waits again (pass 17); one that
/// fails on its own content goes.
pub(crate) fn open_pending_inbound(storage: &Storage, transport: &crate::TransportManager, group_id: &str) -> OmniResult<()> {
    // One drain per group and thread: a Commit opened here asks for another, which only marks that
    // the queue is to be read again — before, it read the whole queue once more inside this one,
    // and a chain of waiting Commits went as deep as the queue is long (blind audit 2026-11-13,
    // pass 19).
    let key = format!("{}|{group_id}", storage.db_path().display());
    let first = DRAINING.with(|draining| {
        let mut draining = draining.borrow_mut();
        match draining.get_mut(&key) {
            Some(again) => {
                *again = true;
                false
            }
            None => {
                draining.insert(key.clone(), false);
                true
            }
        }
    });
    if !first {
        return Ok(());
    }
    struct Draining(String);
    impl Drop for Draining {
        fn drop(&mut self) {
            DRAINING.with(|draining| draining.borrow_mut().remove(&self.0));
        }
    }
    let _draining = Draining(key.clone());
    loop {
        drain_pending_once(storage, transport, group_id)?;
        let again = DRAINING.with(|draining| draining.borrow_mut().get_mut(&key).map(std::mem::take).unwrap_or(false));
        if !again {
            return Ok(());
        }
    }
}

thread_local! {
    /// The groups whose waiting messages this thread is reading, and whether to read them again.
    static DRAINING: std::cell::RefCell<HashMap<String, bool>> = std::cell::RefCell::new(HashMap::new());
}

fn drain_pending_once(storage: &Storage, transport: &crate::TransportManager, group_id: &str) -> OmniResult<()> {
    for (epoch, mls) in storage.list_group_pending_inbound(group_id)? {
        match receive_mls(storage, transport, group_id, &mls, true) {
            Ok(()) => {}
            Err(error @ (OmniError::Storage(_) | OmniError::Internal(_) | OmniError::Transport(_))) => {
                crate::omni_app_trace_log("group/pending", &format!("group={group_id} epoch={epoch} waits again: {error}"));
                // Rolled back, it is still there; failed after its step, it is put back.
                storage.push_group_pending_inbound(group_id, epoch, &mls, current_unix_ms())?;
            }
            Err(error) => {
                crate::omni_app_trace_log("group/pending", &format!("group={group_id} epoch={epoch} dropped: {error}"));
                storage.delete_group_pending_inbound(group_id, &mls)?;
            }
        }
    }
    Ok(())
}

/// A fresh KeyPackage of this profile (what accepting an invitation hands over).
pub(crate) fn own_key_package(storage: &Storage) -> OmniResult<Vec<u8>> {
    with_engine(storage, |engine| engine.key_package())
}

fn send_control_1to1<T: serde::Serialize>(
    storage: &Storage,
    transport: &crate::TransportManager,
    dialog_key: &str,
    kind: GroupControlKind,
    payload: &T,
) -> OmniResult<()> {
    // The session is read and written here without the app session lock (some callers hold it, the
    // public group methods do not), so another sender may advance the ratchet in between. The write
    // is atomic on the revision, which makes that safe — the stale copy is refused, nothing is rolled
    // back — but it would lose this control message. Read the session again and retry instead.
    const ATTEMPTS: usize = 3;
    let mut attempt = 1;
    loop {
        match send_control_1to1_once(storage, transport, dialog_key, kind, payload) {
            Err(error) if attempt < ATTEMPTS && error.to_string().contains(omni_storage::STALE_SESSION_WRITE) => {
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn send_control_1to1_once<T: serde::Serialize>(
    storage: &Storage,
    transport: &crate::TransportManager,
    dialog_key: &str,
    kind: GroupControlKind,
    payload: &T,
) -> OmniResult<()> {
    // Never out of an open transaction: the ratchet step stored with the frame would be taken back by
    // a rollback after the frame is gone, and the next frame sealed with the same step — a re-made
    // invitation inside the transaction of the Commit that called for it did that (blind audit
    // 2026-11-13, pass 18).
    if storage.in_transaction() {
        return Err(OmniError::Internal("a 1:1 group control does not go out of an open transaction".into()));
    }
    let payload_json = serde_json::to_string(payload).map_err(|e| OmniError::Internal(e.to_string()))?;
    let remote_account_id = dialog_account(storage, dialog_key)?
        .ok_or_else(|| OmniError::InvalidInput("group control needs a known contact or our own device".into()))?;
    // The session of the device behind `dialog_key` — the envelope goes out on that dialog.
    let session = storage
        .get_session_for_dialog(&remote_account_id, dialog_key)?
        .ok_or_else(|| OmniError::Transport("no established session for group control".into()))?;
    let event = omni_types::LogicalEventEnvelopeV1::group_control(kind.as_wire(), &payload_json, current_unix_ms());
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &event,
        omni_messaging::SecurityPayloadKind::GroupControl,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    Ok(())
}

/// Whether `dialog_key` is one of our own other devices.
pub(crate) fn own_device_dialog(storage: &Storage, dialog_key: &str) -> OmniResult<bool> {
    let wanted = omni_types::normalize_dialog_key(dialog_key);
    Ok(storage
        .list_own_device_dialogs()?
        .iter()
        .any(|(_, dialog)| omni_types::normalize_dialog_key(dialog) == wanted))
}

/// The account on the other end of `dialog_key`: a contact's, or ours for our own other device.
fn dialog_account(storage: &Storage, dialog_key: &str) -> OmniResult<Option<omni_identity::AccountId>> {
    if let Some(account) = storage.resolve_contact_for_dialog(dialog_key)?.and_then(|contact| contact.remote_account_id) {
        return Ok(Some(account));
    }
    if own_device_dialog(storage, dialog_key)? {
        return Ok(storage.get_local_account()?.map(|account| account.descriptor.account_id));
    }
    Ok(None)
}

/// Tell our other device on `dialog_key` it is not in this group yet (it answers with a `device_join`).
pub(crate) fn send_device_offer(
    storage: &Storage,
    transport: &crate::TransportManager,
    dialog_key: &str,
    offer: &GroupInviteV2,
) -> OmniResult<()> {
    send_control_1to1(storage, transport, dialog_key, GroupControlKind::DeviceOffer, offer)
}

/// Ask the contact on `dialog_key` to add this device to a group its account is in.
pub(crate) fn send_device_join(
    storage: &Storage,
    transport: &crate::TransportManager,
    dialog_key: &str,
    request: &GroupInviteAcceptV2,
) -> OmniResult<()> {
    send_control_1to1(storage, transport, dialog_key, GroupControlKind::DeviceJoin, request)
}

/// Control-kind wire mapping (the app owns the 1:1 consent semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupControlKind {
    Invite,
    InviteAccept,
    InviteDecline,
    Welcome,
    /// A device of a member's account asks to be added (payload: an accept with its KeyPackage).
    DeviceJoin,
    /// To our own other device: "you are not in this group" (payload: an invite, never shown).
    DeviceOffer,
}

impl GroupControlKind {
    fn as_wire(self) -> &'static str {
        match self {
            GroupControlKind::Invite => "invite",
            GroupControlKind::InviteAccept => "invite_accept",
            GroupControlKind::InviteDecline => "invite_decline",
            GroupControlKind::Welcome => "welcome",
            GroupControlKind::DeviceJoin => "device_join",
            GroupControlKind::DeviceOffer => "device_offer",
        }
    }
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "invite" => Some(GroupControlKind::Invite),
            "invite_accept" => Some(GroupControlKind::InviteAccept),
            "invite_decline" => Some(GroupControlKind::InviteDecline),
            "welcome" => Some(GroupControlKind::Welcome),
            "device_join" => Some(GroupControlKind::DeviceJoin),
            "device_offer" => Some(GroupControlKind::DeviceOffer),
            _ => None,
        }
    }
}
