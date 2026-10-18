//! Group crypto and membership on MLS (RFC 9420) via OpenMLS — docs/decisions/group-crypto-mls.md.
//!
//! What OpenMLS does: the ratchet tree, epochs, Welcome, encryption, signatures. What this crate
//! adds, because MLS leaves it to the deployment:
//!
//! - **The order of Commits without a server.** MLS needs exactly one Commit per epoch; a delivery
//!   server usually sequences them. Our durable feeds deliver everything to everyone in any order, so
//!   among Commits competing for epoch E the one with the smallest SHA-256 over its wire bytes wins —
//!   everywhere, whatever the arrival order. Before merging a Commit a member snapshots the group's
//!   rows; a stronger competitor arriving later restores the snapshot and merges the winner instead.
//! - **Who may do what.** Group name and admins live in a group-context extension; every member checks
//!   a Commit against it BEFORE merging: anyone adds, admins remove others and change name/admins,
//!   anyone leaves (a self-remove proposal that any member then commits).
//! - **Who is who.** A member signs MLS with its ACCOUNT key (Ed25519, the same family), so the leaf
//!   signature itself proves which account a leaf belongs to — no separate binding to forge. MLS
//!   signatures are domain-separated (`SignWithLabel`, "MLS 1.0 …"), so they cannot be mistaken for
//!   any other signature the account makes. The credential carries the member's card.
//!
//! Storage is the OpenMLS SQLite provider on OUR connection, with a codec chosen by the caller (the
//! app encrypts at rest). Pure Rust, no platform code: identical bytes on every platform.

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{OpenMlsRustCrypto, RustCrypto};
use openmls_sqlite_storage::SqliteStorageProvider;
use openmls_traits::OpenMlsProvider;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use openmls_sqlite_storage::Codec;

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Group-context extension carrying [`GroupMeta`] (a private-use extension type).
pub const OMNI_GROUP_META_EXTENSION: u16 = 0xff0a;

/// How many past epochs a member keeps secrets for, so a message sent just before a Commit but
/// arriving just after it (store-and-forward reorders freely) still opens.
const MAX_PAST_EPOCHS: usize = 8;

/// Epochs of our branch remembered by Commit rank (see `omni_mls_commit_log`).
const COMMIT_LOG_EPOCHS: i64 = 256;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("{0}")]
    Mls(String),
    #[error("storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("not allowed: {0}")]
    NotAllowed(&'static str),
    #[error("unknown group")]
    UnknownGroup,
    #[error("invalid input: {0}")]
    Invalid(String),
}

fn mls<E: std::fmt::Display>(error: E) -> GroupError {
    GroupError::Mls(error.to_string())
}

pub type Result<T> = std::result::Result<T, GroupError>;

/// Who a member is, carried in its MLS credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberCard {
    /// Account Ed25519 public key, base64 — also the MLS signature key of the member's leaf.
    pub member_pub: String,
    pub account_id: String,
    pub display_name: String,
}

/// Group name and admins — what every member authorizes Commits against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMeta {
    pub name: String,
    /// `member_pub`s of the admins. The creator is the first.
    pub admins: Vec<String>,
    /// The group's picture, a small JPEG in base64 (set by an admin, like the name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// A newcomer gets the recent history (docs/decisions/group-history-sync.md): on unless an admin
    /// turned it off (owner decision 2026-10-11).
    #[serde(default = "newcomer_history_default", skip_serializing_if = "is_newcomer_history_default")]
    pub newcomer_history: bool,
}

fn newcomer_history_default() -> bool {
    true
}

fn is_newcomer_history_default(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub card: MemberCard,
    pub is_admin: bool,
    pub is_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupView {
    pub group_id: Vec<u8>,
    pub name: String,
    /// The picture from the meta (base64 JPEG), if any.
    pub avatar: Option<String>,
    /// Newcomers get the recent history (the meta's switch).
    pub newcomer_history: bool,
    pub epoch: u64,
    pub members: Vec<MemberView>,
    /// False once we were removed (or left and someone committed it).
    pub active: bool,
}

/// Result of adding members: the Commit for the group, the Welcome for the newcomers.
#[derive(Debug)]
pub struct AddOutput {
    pub commit: Vec<u8>,
    pub welcome: Vec<u8>,
    pub added: Vec<MemberCard>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The Commit for our current epoch: merged.
    Merged,
    /// A competitor for the epoch we had already merged into, and the stronger one: we rolled back and
    /// merged it. `our_commit_lost` — the Commit rolled back was OURS: re-issue our change.
    ReplacedLoser { our_commit_lost: bool },
    /// A competitor that lost to the Commit we merged: ignored.
    LostIgnored,
    /// The Commit breaks the group's rules (e.g. a non-admin removing someone): not merged.
    Rejected(String),
}

#[derive(Debug)]
pub enum Inbound {
    /// A decrypted application message.
    Application { sender: MemberCard, epoch: u64, payload: Vec<u8> },
    /// A Commit was handled; `view` is the group after it. `put_back` — members it put back after
    /// their branch lost (their old leaf out, the same member in).
    Commit { outcome: CommitOutcome, view: GroupView, put_back: Vec<String> },
    /// For an epoch we have not reached: keep the bytes, offer them again after the next Commit.
    Future { epoch: u64 },
    /// A Commit for an epoch we passed long ago, not the one we merged there: the group went on in
    /// two branches (a network split, two devices apart). `ours_wins` — our Commit at `epoch` ranks
    /// lower (wins by the same rule as a competitor one epoch back); otherwise our branch lost and we
    /// must come back into the other one (docs/decisions/group-history-sync.md, «Синхронизация эпох»).
    ForkSeen { epoch: u64, ours_wins: bool },
    /// A member asks to be put back into the group with this KeyPackage: its branch lost. `card` is
    /// checked against the key that signed the KeyPackage, and is a member of the group here.
    RejoinRequest { card: MemberCard, key_package: Vec<u8> },
    /// A Welcome on the group's feed — for whoever it is for (a member that asked to come back).
    Welcome(Vec<u8>),
    Ignored(String),
}

/// SHA-256 rank of a Commit's wire bytes: the smaller wins its epoch.
pub fn commit_rank(commit_wire: &[u8]) -> [u8; 32] {
    Sha256::digest(commit_wire).into()
}

fn wire(message: &MlsMessageOut) -> Result<Vec<u8>> {
    message.tls_serialize_detached().map_err(mls)
}

/// The OpenMLS provider over our SQLite connection.
pub struct Provider<'c, C: Codec> {
    rust_crypto: OpenMlsRustCrypto,
    storage: SqliteStorageProvider<C, &'c Connection>,
}

impl<'c, C: Codec> OpenMlsProvider for Provider<'c, C> {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = SqliteStorageProvider<C, &'c Connection>;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }
    fn crypto(&self) -> &Self::CryptoProvider {
        self.rust_crypto.crypto()
    }
    fn rand(&self) -> &Self::RandProvider {
        self.rust_crypto.rand()
    }
}

/// Tables OpenMLS keeps per group (keyed by the codec-encoded group id) — what a snapshot copies.
const GROUP_TABLES: [&str; 4] = [
    "openmls_group_data",
    "openmls_epoch_keys_pairs",
    "openmls_own_leaf_nodes",
    "openmls_proposals",
];
/// Leaf encryption keys are keyed by public key, not group; a snapshot keeps them all and a restore
/// only re-inserts missing ones (a merge may have deleted the old ones).
const SHARED_KEY_TABLE: &str = "openmls_encryption_keys";

/// Rows of one table, schema-agnostic (column names + values), JSON-serializable.
#[derive(Serialize, Deserialize)]
struct TableRows {
    table: String,
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}

#[derive(Serialize, Deserialize)]
enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<rusqlite::types::Value> for SqlValue {
    fn from(value: rusqlite::types::Value) -> Self {
        use rusqlite::types::Value;
        match value {
            Value::Null => SqlValue::Null,
            Value::Integer(i) => SqlValue::Int(i),
            Value::Real(r) => SqlValue::Real(r),
            Value::Text(t) => SqlValue::Text(t),
            Value::Blob(b) => SqlValue::Blob(b),
        }
    }
}

impl rusqlite::ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, ValueRef};
        Ok(match self {
            SqlValue::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            SqlValue::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            SqlValue::Real(r) => ToSqlOutput::Borrowed(ValueRef::Real(*r)),
            SqlValue::Text(t) => ToSqlOutput::Borrowed(ValueRef::Text(t.as_bytes())),
            SqlValue::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
        })
    }
}

/// One member's MLS engine for its groups, over a SQLite connection.
pub struct Engine<'c, C: Codec> {
    conn: &'c Connection,
    provider: Provider<'c, C>,
    signer: SignatureKeyPair,
    me: MemberCard,
}

impl<'c, C: Codec> Engine<'c, C> {
    /// Create OpenMLS's tables and ours (idempotent). Run once per database before [`Engine::new`].
    pub fn migrate(conn: &mut Connection) -> Result<()> {
        SqliteStorageProvider::<C, &mut Connection>::new(conn)
            .run_migrations()
            .map_err(mls)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS omni_mls_snapshots (
                group_id BLOB PRIMARY KEY NOT NULL,
                epoch INTEGER NOT NULL,
                merged_rank BLOB NOT NULL,
                ours INTEGER NOT NULL,
                rows TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS omni_mls_commit_log (
                group_id BLOB NOT NULL,
                epoch INTEGER NOT NULL,
                rank BLOB NOT NULL,
                PRIMARY KEY (group_id, epoch)
            );",
        )?;
        Ok(())
    }

    /// `signing_seed` — the account's Ed25519 secret (32 bytes); `me.member_pub` must be its public
    /// key in base64.
    pub fn new(conn: &'c Connection, signing_seed: [u8; 32], me: MemberCard) -> Self {
        let public = ed25519_dalek::SigningKey::from_bytes(&signing_seed)
            .verifying_key()
            .to_bytes()
            .to_vec();
        let signer = SignatureKeyPair::from_raw(SignatureScheme::ED25519, signing_seed.to_vec(), public);
        Self {
            conn,
            provider: Provider {
                rust_crypto: OpenMlsRustCrypto::default(),
                storage: SqliteStorageProvider::new(conn),
            },
            signer,
            me,
        }
    }

    fn credential(&self) -> Result<CredentialWithKey> {
        let card = serde_json::to_vec(&self.me).map_err(mls)?;
        Ok(CredentialWithKey {
            credential: BasicCredential::new(card).into(),
            signature_key: self.signer.public().into(),
        })
    }

    fn capabilities() -> Capabilities {
        Capabilities::new(
            None,
            Some(&[CIPHERSUITE]),
            Some(&[ExtensionType::Unknown(OMNI_GROUP_META_EXTENSION)]),
            None,
            None,
        )
    }

    /// A fresh KeyPackage — what an invitee hands over when accepting an invitation.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(Self::capabilities())
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential()?)
            .map_err(mls)?;
        bundle.key_package().tls_serialize_detached().map_err(mls)
    }

    fn load(&self, group_id: &[u8]) -> Result<MlsGroup> {
        MlsGroup::load(self.provider.storage(), &GroupId::from_slice(group_id))
            .map_err(mls)?
            .ok_or(GroupError::UnknownGroup)
    }

    /// The group context: our meta extension, declared REQUIRED so every member must support it.
    fn context_extensions(meta: &GroupMeta) -> Result<Extensions<GroupContext>> {
        Extensions::from_vec(vec![
            Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[ExtensionType::Unknown(OMNI_GROUP_META_EXTENSION)],
                &[],
                &[],
            )),
            Extension::Unknown(
                OMNI_GROUP_META_EXTENSION,
                UnknownExtension(serde_json::to_vec(meta).map_err(mls)?),
            ),
        ])
        .map_err(mls)
    }

    fn meta_of(group: &MlsGroup) -> Result<GroupMeta> {
        let ext = group
            .extensions()
            .unknown(OMNI_GROUP_META_EXTENSION)
            .ok_or_else(|| GroupError::Invalid("group has no meta extension".into()))?;
        serde_json::from_slice(&ext.0).map_err(mls)
    }

    fn card_of(credential: &Credential) -> Option<MemberCard> {
        let basic = BasicCredential::try_from(credential.clone()).ok()?;
        serde_json::from_slice(basic.identity()).ok()
    }

    /// The card must belong to the key that signs the leaf — otherwise anyone could claim a name.
    fn check_card(credential: &Credential, signature_key: &[u8]) -> Result<MemberCard> {
        let card = Self::card_of(credential).ok_or_else(|| GroupError::Invalid("unreadable member card".into()))?;
        let claimed = base64_decode(&card.member_pub)
            .ok_or_else(|| GroupError::Invalid("member_pub is not base64".into()))?;
        if claimed != signature_key {
            return Err(GroupError::Invalid("member card does not match its signing key".into()));
        }
        Ok(card)
    }

    /// Create a group with us as its only member and admin.
    pub fn create_group(&self, group_id: &[u8], name: &str) -> Result<()> {
        let meta = GroupMeta {
            name: name.to_string(),
            admins: vec![self.me.member_pub.clone()],
            avatar: None,
            newcomer_history: true,
        };
        MlsGroup::builder()
            .ciphersuite(CIPHERSUITE)
            .with_group_id(GroupId::from_slice(group_id))
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .with_capabilities(Self::capabilities())
            .with_group_context_extensions(Self::context_extensions(&meta)?)
            .build(&self.provider, &self.signer, self.credential()?)
            .map_err(mls)?;
        Ok(())
    }

    pub fn view(&self, group_id: &[u8]) -> Result<GroupView> {
        let group = self.load(group_id)?;
        self.view_of(&group)
    }

    fn view_of(&self, group: &MlsGroup) -> Result<GroupView> {
        let meta = Self::meta_of(group)?;
        let mut members: Vec<MemberView> = group
            .members()
            .filter_map(|m| Self::card_of(&m.credential))
            .map(|card| MemberView {
                is_admin: meta.admins.contains(&card.member_pub),
                is_me: card.member_pub == self.me.member_pub,
                card,
            })
            .collect();
        members.sort_by(|a, b| a.card.display_name.cmp(&b.card.display_name));
        Ok(GroupView {
            group_id: group.group_id().as_slice().to_vec(),
            name: meta.name,
            avatar: meta.avatar,
            newcomer_history: meta.newcomer_history,
            epoch: group.epoch().as_u64(),
            members,
            active: group.is_active(),
        })
    }

    /// Add members by their KeyPackages (anyone may add). The Commit goes to the group, the Welcome to
    /// the newcomers.
    pub fn add_members(&self, group_id: &[u8], key_packages: &[Vec<u8>]) -> Result<AddOutput> {
        let mut kps = Vec::new();
        let mut added = Vec::new();
        for bytes in key_packages {
            let kp = KeyPackageIn::tls_deserialize_exact(bytes)
                .map_err(mls)?
                .validate(self.provider.crypto(), ProtocolVersion::Mls10)
                .map_err(mls)?;
            added.push(Self::check_card(
                kp.leaf_node().credential(),
                kp.leaf_node().signature_key().as_slice(),
            )?);
            kps.push(kp);
        }
        let mut group = self.load(group_id)?;
        let before = self.snapshot_rows(group_id)?;
        let (commit, welcome, _info) =
            group.add_members(&self.provider, &self.signer, &kps).map_err(mls)?;
        group.merge_pending_commit(&self.provider).map_err(mls)?;
        let commit = wire(&commit)?;
        self.save_snapshot(group_id, group.epoch().as_u64(), &commit_rank(&commit), true, &before)?;
        Ok(AddOutput { commit, welcome: wire(&welcome)?, added })
    }

    /// Join from a Welcome; returns the group id.
    pub fn join(&self, welcome: &[u8]) -> Result<Vec<u8>> {
        let welcome = match MlsMessageIn::tls_deserialize_exact(welcome).map_err(mls)?.extract() {
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            _ => return Err(GroupError::Invalid("not a Welcome".into())),
        };
        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .build();
        let group = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(mls)?
            .into_group(&self.provider)
            .map_err(mls)?;
        // Every leaf's card must match its signing key — a forged card in the tree is refused.
        for member in group.members() {
            Self::check_card(&member.credential, &member.signature_key)?;
        }
        Self::meta_of(&group)?;
        let group_id = group.group_id().as_slice().to_vec();
        self.conn
            .execute("DELETE FROM omni_mls_snapshots WHERE group_id = ?1", params![group_id])?;
        Ok(group_id)
    }

    fn admin_check(&self, group: &MlsGroup) -> Result<GroupMeta> {
        let meta = Self::meta_of(group)?;
        if !meta.admins.contains(&self.me.member_pub) {
            return Err(GroupError::NotAllowed("only an admin may do this"));
        }
        Ok(meta)
    }

    /// Remove members (admins only). Returns the Commit.
    pub fn remove_members(&self, group_id: &[u8], member_pubs: &[String]) -> Result<Vec<u8>> {
        let mut group = self.load(group_id)?;
        let mut meta = self.admin_check(&group)?;
        let indices: Vec<LeafNodeIndex> = group
            .members()
            .filter(|m| Self::card_of(&m.credential).is_some_and(|c| member_pubs.contains(&c.member_pub)))
            .map(|m| m.index)
            .collect();
        if indices.is_empty() {
            return Err(GroupError::Invalid("no such member".into()));
        }
        // A removed admin stops being one.
        meta.admins.retain(|a| !member_pubs.contains(a));
        let before = self.snapshot_rows(group_id)?;
        let (commit, _welcome, _info) =
            group.remove_members(&self.provider, &self.signer, &indices).map_err(mls)?;
        group.merge_pending_commit(&self.provider).map_err(mls)?;
        let commit = wire(&commit)?;
        self.save_snapshot(group_id, group.epoch().as_u64(), &commit_rank(&commit), true, &before)?;
        Ok(commit)
    }

    /// Change the group name and/or admins (admins only). Returns the Commit.
    pub fn update_meta(&self, group_id: &[u8], change: impl FnOnce(&mut GroupMeta)) -> Result<Vec<u8>> {
        let mut group = self.load(group_id)?;
        let mut meta = self.admin_check(&group)?;
        change(&mut meta);
        if meta.admins.is_empty() {
            return Err(GroupError::Invalid("a group keeps at least one admin".into()));
        }
        let before = self.snapshot_rows(group_id)?;
        let (commit, _welcome, _info) = group
            .update_group_context_extensions(
                &self.provider,
                Self::context_extensions(&meta)?,
                &self.signer,
            )
            .map_err(mls)?;
        group.merge_pending_commit(&self.provider).map_err(mls)?;
        let commit = wire(&commit)?;
        self.save_snapshot(group_id, group.epoch().as_u64(), &commit_rank(&commit), true, &before)?;
        Ok(commit)
    }

    /// May we leave? The last admin of a group that still has others must hand over first. Leaving
    /// itself is the app's: a signed "leaving" application message, then [`Engine::delete_group`]; an
    /// admin then removes us with an ordinary Commit. Not an MLS self-remove proposal: a proposal
    /// blocks every member from sending until somebody commits it, and the committer may be offline
    /// for days — the group would freeze.
    pub fn can_leave(&self, group_id: &[u8]) -> Result<()> {
        let group = self.load(group_id)?;
        let meta = Self::meta_of(&group)?;
        if meta.admins == [self.me.member_pub.clone()] && group.members().count() > 1 {
            return Err(GroupError::NotAllowed("the last admin must name another admin before leaving"));
        }
        Ok(())
    }

    /// Encrypt an application message. Returns (epoch, wire).
    pub fn encrypt(&self, group_id: &[u8], payload: &[u8]) -> Result<(u64, Vec<u8>)> {
        let mut group = self.load(group_id)?;
        let out = group.create_message(&self.provider, &self.signer, payload).map_err(mls)?;
        Ok((group.epoch().as_u64(), wire(&out)?))
    }

    /// Forget a group locally (we were removed, or the user deleted it).
    pub fn delete_group(&self, group_id: &[u8]) -> Result<()> {
        if let Ok(mut group) = self.load(group_id) {
            group.delete(self.provider.storage()).map_err(mls)?;
        }
        self.conn
            .execute("DELETE FROM omni_mls_snapshots WHERE group_id = ?1", params![group_id])?;
        self.conn
            .execute("DELETE FROM omni_mls_commit_log WHERE group_id = ?1", params![group_id])?;
        Ok(())
    }

    /// The rank of the Commit our branch merged at `epoch`, if we remember it.
    pub fn branch_rank(&self, group_id: &[u8], epoch: u64) -> Result<Option<[u8; 32]>> {
        let rank: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT rank FROM omni_mls_commit_log WHERE group_id = ?1 AND epoch = ?2",
                params![group_id, epoch as i64],
                |row| row.get(0),
            )
            .optional()?;
        Ok(rank.and_then(|rank| rank.try_into().ok()))
    }

    /// Our branch lost: a fresh KeyPackage, as an MLS message for the group's feed — a member of the
    /// winning branch puts us back with it ([`Engine::readd_member`]).
    pub fn rejoin_request(&self) -> Result<Vec<u8>> {
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(Self::capabilities())
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential()?)
            .map_err(mls)?;
        wire(&MlsMessageOut::from(bundle.key_package().clone()))
    }

    /// Put a member whose branch lost back into the group: its old leaf (which it can no longer use)
    /// goes, the KeyPackage it asked with comes in — one Commit, anyone may make it (the rule allows a
    /// removal that re-adds the same member).
    pub fn readd_member(&self, group_id: &[u8], key_package: &[u8]) -> Result<AddOutput> {
        let kp = KeyPackageIn::tls_deserialize_exact(key_package)
            .map_err(mls)?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(mls)?;
        let card = Self::check_card(kp.leaf_node().credential(), kp.leaf_node().signature_key().as_slice())?;
        let mut group = self.load(group_id)?;
        let old: Vec<LeafNodeIndex> = group
            .members()
            .filter(|m| Self::card_of(&m.credential).is_some_and(|c| c.member_pub == card.member_pub))
            .map(|m| m.index)
            .collect();
        if old.len() != 1 || card.member_pub == self.me.member_pub {
            return Err(GroupError::Invalid("not a member to put back".into()));
        }
        let before = self.snapshot_rows(group_id)?;
        let messages = group.swap_members(&self.provider, &self.signer, &old, &[kp]).map_err(mls)?;
        group.merge_pending_commit(&self.provider).map_err(mls)?;
        let (commit, welcome) = (messages.commit, messages.welcome);
        let commit = wire(&commit)?;
        self.save_snapshot(group_id, group.epoch().as_u64(), &commit_rank(&commit), true, &before)?;
        Ok(AddOutput { commit, welcome: wire(&welcome)?, added: vec![card] })
    }

    /// Who a KeyPackage would bring in — its card checked against the leaf's signing key.
    pub fn key_package_card(&self, key_package: &[u8]) -> Result<MemberCard> {
        let kp = KeyPackageIn::tls_deserialize_exact(key_package)
            .map_err(mls)?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(mls)?;
        Self::check_card(kp.leaf_node().credential(), kp.leaf_node().signature_key().as_slice())
    }

    /// Whether the member this KeyPackage is for is already in the group with it (somebody put it
    /// back already).
    pub fn has_leaf_of(&self, group_id: &[u8], key_package: &[u8]) -> Result<bool> {
        let kp = KeyPackageIn::tls_deserialize_exact(key_package)
            .map_err(mls)?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(mls)?;
        // The key on the wire is length-prefixed; the member list holds it bare.
        let wanted = kp.leaf_node().encryption_key().tls_serialize_detached().map_err(mls)?;
        let group = self.load(group_id)?;
        let found = group.members().any(|member| {
            !member.encryption_key.is_empty()
                && wanted.ends_with(&member.encryption_key)
                && wanted.len() - member.encryption_key.len() <= 4
        });
        Ok(found)
    }

    /// Come back into the group from a Welcome that puts us back (our branch lost). Our old state of
    /// the group goes only if the Welcome is really for us; otherwise nothing changes.
    pub fn rejoin(&self, welcome: &[u8]) -> Result<Vec<u8>> {
        let welcome = match MlsMessageIn::tls_deserialize_exact(welcome).map_err(mls)?.extract() {
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            _ => return Err(GroupError::Invalid("not a Welcome".into())),
        };
        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .build();
        // Opening the Welcome needs the KeyPackage it was made for: one that is not for us fails here,
        // before anything is touched.
        let processed = ProcessedWelcome::new_from_welcome(&self.provider, &config, welcome).map_err(mls)?;
        let group_id = processed.unverified_group_info().group_id().as_slice().to_vec();
        // OpenMLS refuses a Welcome into a group it holds: keep our state aside, drop it, join — and
        // put our state back if the Welcome does not hold up.
        let before = self.snapshot_rows(&group_id)?;
        if let Ok(mut old) = self.load(&group_id) {
            old.delete(self.provider.storage()).map_err(mls)?;
        }
        let joined = processed
            .into_staged_welcome(&self.provider, None)
            .map_err(mls)
            .and_then(|staged| staged.into_group(&self.provider).map_err(mls))
            .and_then(|group| {
                for member in group.members() {
                    Self::check_card(&member.credential, &member.signature_key)?;
                }
                Self::meta_of(&group)?;
                Ok(group)
            });
        match joined {
            Ok(_) => {
                self.conn
                    .execute("DELETE FROM omni_mls_snapshots WHERE group_id = ?1", params![group_id])?;
                self.conn
                    .execute("DELETE FROM omni_mls_commit_log WHERE group_id = ?1", params![group_id])?;
                Ok(group_id)
            }
            Err(error) => {
                self.restore_rows(&group_id, &before)?;
                Err(error)
            }
        }
    }

    /// Process anything that arrived on the group's feeds.
    pub fn receive(&self, group_id: &[u8], bytes: &[u8]) -> Result<Inbound> {
        let Ok(incoming) = MlsMessageIn::tls_deserialize_exact(bytes) else {
            return Ok(Inbound::Ignored("not an MLS message".into()));
        };
        let message = match incoming.extract() {
            MlsMessageBodyIn::PublicMessage(message) => ProtocolMessage::from(message),
            MlsMessageBodyIn::PrivateMessage(message) => ProtocolMessage::from(message),
            MlsMessageBodyIn::Welcome(_) => return Ok(Inbound::Welcome(bytes.to_vec())),
            MlsMessageBodyIn::KeyPackage(key_package) => return self.rejoin_request_in(group_id, key_package),
            _ => return Ok(Inbound::Ignored("not a protocol message".into())),
        };
        if message.group_id().as_slice() != group_id {
            return Ok(Inbound::Ignored("another group".into()));
        }
        let mut group = self.load(group_id)?;
        let current = group.epoch().as_u64();
        let epoch = message.epoch().as_u64();
        let is_commit = matches!(message.content_type(), ContentType::Commit);

        if is_commit {
            if epoch == current {
                return self.merge_commit(group, message, bytes, CommitOutcome::Merged);
            }
            if epoch + 1 == current {
                // A competitor for the epoch we already merged into.
                let Some((merged_rank, ours, rows)) = self.load_snapshot(group_id, current)? else {
                    return self.older_commit(group_id, epoch, current, bytes);
                };
                let rank = commit_rank(bytes);
                if rank == merged_rank {
                    return Ok(Inbound::Ignored("the commit we already merged".into()));
                }
                if rank > merged_rank {
                    return Ok(Inbound::Commit {
                        outcome: CommitOutcome::LostIgnored,
                        view: self.view_of(&group)?,
                        put_back: Vec::new(),
                    });
                }
                // Keep where we are: a "competitor" from a branch that split earlier does not build on
                // our parent either, and must not leave us rolled back (it is told apart by the
                // Commit of the epoch the branches split at, see `older_commit`).
                let where_we_are = self.snapshot_rows(group_id)?;
                self.restore_rows(group_id, &rows)?;
                let group = self.load(group_id)?;
                let merged = self.merge_commit(
                    group,
                    message,
                    bytes,
                    CommitOutcome::ReplacedLoser { our_commit_lost: ours },
                )?;
                if let Inbound::Commit { outcome: CommitOutcome::Rejected(reason), .. } = &merged {
                    let reason = reason.clone();
                    self.restore_rows(group_id, &where_we_are)?;
                    return Ok(Inbound::Ignored(format!("a competitor that does not build on our parent: {reason}")));
                }
                return Ok(merged);
            }
            if epoch > current {
                return Ok(Inbound::Future { epoch });
            }
            // Older than our snapshot window: if it is not what our branch merged at that epoch, the
            // group went on in two branches — who wins is decided at the epoch they split.
            return self.older_commit(group_id, epoch, current, bytes);
        }
        if epoch > current {
            return Ok(Inbound::Future { epoch });
        }
        let processed = match group.process_message(&self.provider, message) {
            Ok(processed) => processed,
            Err(error) => return Ok(Inbound::Ignored(format!("process: {error}"))),
        };
        let sender = Self::card_of(processed.credential());
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => match sender {
                Some(sender) => Ok(Inbound::Application { sender, epoch, payload: app.into_bytes() }),
                None => Ok(Inbound::Ignored("application message without a card".into())),
            },
            // This protocol sends no proposals (see `can_leave`): one arriving is not ours to act on.
            ProcessedMessageContent::ProposalMessage(_) => Ok(Inbound::Ignored("proposal".into())),
            ProcessedMessageContent::StagedCommitMessage(_) => {
                Ok(Inbound::Ignored("commit outside the commit path".into()))
            }
            other => Ok(Inbound::Ignored(format!("unexpected content {other:?}"))),
        }
    }

    /// A Commit for an epoch we passed, beyond rolling back.
    fn older_commit(&self, group_id: &[u8], epoch: u64, current: u64, bytes: &[u8]) -> Result<Inbound> {
        let rank = commit_rank(bytes);
        match self.branch_rank(group_id, epoch)? {
            Some(ours) if ours == rank => Ok(Inbound::Ignored("a commit of our own branch".into())),
            Some(ours) => Ok(Inbound::ForkSeen { epoch, ours_wins: ours < rank }),
            None => Ok(Inbound::Ignored(format!("commit for epoch {epoch}, we are at {current}"))),
        }
    }

    /// A KeyPackage on the feed: a member asking to be put back. Only a current member of this group
    /// (other than us) with a valid KeyPackage of its own key counts.
    fn rejoin_request_in(&self, group_id: &[u8], key_package: KeyPackageIn) -> Result<Inbound> {
        let Ok(kp) = key_package.validate(self.provider.crypto(), ProtocolVersion::Mls10) else {
            return Ok(Inbound::Ignored("an invalid key package".into()));
        };
        let Ok(card) = Self::check_card(kp.leaf_node().credential(), kp.leaf_node().signature_key().as_slice()) else {
            return Ok(Inbound::Ignored("a key package with a forged card".into()));
        };
        let group = self.load(group_id)?;
        let member = group
            .members()
            .any(|m| Self::card_of(&m.credential).is_some_and(|c| c.member_pub == card.member_pub));
        if !member || card.member_pub == self.me.member_pub {
            return Ok(Inbound::Ignored("a key package of no member to put back".into()));
        }
        let key_package = kp.tls_serialize_detached().map_err(mls)?;
        Ok(Inbound::RejoinRequest { card, key_package })
    }

    fn merge_commit(
        &self,
        mut group: MlsGroup,
        message: ProtocolMessage,
        bytes: &[u8],
        outcome: CommitOutcome,
    ) -> Result<Inbound> {
        let group_id = group.group_id().as_slice().to_vec();
        let before = self.snapshot_rows(&group_id)?;
        let processed = match group.process_message(&self.provider, message) {
            Ok(processed) => processed,
            Err(error) => {
                return Ok(Inbound::Commit {
                    outcome: CommitOutcome::Rejected(format!("process: {error}")),
                    view: self.view_of(&group)?,
                    put_back: Vec::new(),
                })
            }
        };
        let committer = match processed.sender() {
            Sender::Member(index) => group.member(*index).and_then(Self::card_of),
            _ => None,
        };
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Ok(Inbound::Ignored("not a commit".into()));
        };
        if let Err(reason) = self.authorize(&group, committer.as_ref(), &staged) {
            // Restore what processing staged (nothing merged yet, but keep the store clean).
            self.restore_rows(&group_id, &before)?;
            let group = self.load(&group_id)?;
            return Ok(Inbound::Commit {
                outcome: CommitOutcome::Rejected(reason),
                view: self.view_of(&group)?,
                put_back: Vec::new(),
            });
        }
        // New members' cards must match their keys.
        for add in staged.add_proposals() {
            let leaf = add.add_proposal().key_package().leaf_node();
            if let Err(error) = Self::check_card(leaf.credential(), leaf.signature_key().as_slice()) {
                self.restore_rows(&group_id, &before)?;
                let group = self.load(&group_id)?;
                return Ok(Inbound::Commit {
                    outcome: CommitOutcome::Rejected(error.to_string()),
                    view: self.view_of(&group)?,
                    put_back: Vec::new(),
                });
            }
        }
        let removed_pubs: Vec<String> = staged
            .remove_proposals()
            .filter_map(|remove| group.member(remove.remove_proposal().removed()).and_then(Self::card_of))
            .map(|card| card.member_pub)
            .collect();
        let put_back: Vec<String> = staged
            .add_proposals()
            .filter_map(|add| Self::card_of(add.add_proposal().key_package().leaf_node().credential()))
            .map(|card| card.member_pub)
            .filter(|member| removed_pubs.contains(member))
            .collect();
        group.merge_staged_commit(&self.provider, *staged).map_err(mls)?;
        self.save_snapshot(&group_id, group.epoch().as_u64(), &commit_rank(bytes), false, &before)?;
        Ok(Inbound::Commit { outcome, view: self.view_of(&group)?, put_back })
    }

    /// The group's rules, checked before a Commit is merged: anyone adds; removing someone else and
    /// changing name/admins take an admin (as of the epoch the Commit builds on); leaving is free.
    fn authorize(
        &self,
        group: &MlsGroup,
        committer: Option<&MemberCard>,
        staged: &StagedCommit,
    ) -> std::result::Result<(), String> {
        let meta = Self::meta_of(group).map_err(|e| e.to_string())?;
        let is_admin = |card: Option<&MemberCard>| card.is_some_and(|c| meta.admins.contains(&c.member_pub));
        // A member put back after its branch lost: its old leaf goes and the same member comes in.
        let readded: Vec<String> = staged
            .add_proposals()
            .filter_map(|add| Self::card_of(add.add_proposal().key_package().leaf_node().credential()))
            .map(|card| card.member_pub)
            .collect();
        for remove in staged.remove_proposals() {
            let removed = remove.remove_proposal().removed();
            let by = match remove.sender() {
                Sender::Member(index) => Some(*index),
                _ => None,
            };
            let self_removal = by == Some(removed);
            let put_back = group
                .member(removed)
                .and_then(Self::card_of)
                .is_some_and(|card| readded.contains(&card.member_pub));
            if !self_removal && !put_back && !is_admin(committer) {
                return Err("a non-admin removed a member".into());
            }
        }
        let changes_meta = staged
            .queued_proposals()
            .any(|p| matches!(p.proposal(), Proposal::GroupContextExtensions(_)));
        if changes_meta && !is_admin(committer) {
            return Err("a non-admin changed the group name or admins".into());
        }
        Ok(())
    }

    // --- snapshots -------------------------------------------------------------------------------

    fn group_key(group_id: &[u8]) -> Result<Vec<u8>> {
        C::to_vec(&GroupId::from_slice(group_id)).map_err(mls)
    }

    fn snapshot_rows(&self, group_id: &[u8]) -> Result<Vec<TableRows>> {
        let key = Self::group_key(group_id)?;
        let mut out = Vec::new();
        for table in GROUP_TABLES {
            out.push(self.select_rows(table, Some(&key))?);
        }
        out.push(self.select_rows(SHARED_KEY_TABLE, None)?);
        Ok(out)
    }

    fn select_rows(&self, table: &str, group_key: Option<&[u8]>) -> Result<TableRows> {
        let sql = match group_key {
            Some(_) => format!("SELECT * FROM {table} WHERE group_id = ?1"),
            None => format!("SELECT * FROM {table}"),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let n = columns.len();
        let map = |row: &rusqlite::Row| -> rusqlite::Result<Vec<SqlValue>> {
            (0..n).map(|i| row.get::<_, rusqlite::types::Value>(i).map(SqlValue::from)).collect()
        };
        let rows = match group_key {
            Some(key) => stmt.query_map(params![key], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(TableRows { table: table.to_string(), columns, rows })
    }

    fn restore_rows(&self, group_id: &[u8], tables: &[TableRows]) -> Result<()> {
        let key = Self::group_key(group_id)?;
        let tx = self.conn.unchecked_transaction()?;
        for t in tables {
            let shared = t.table == SHARED_KEY_TABLE;
            if !shared {
                tx.execute(&format!("DELETE FROM {} WHERE group_id = ?1", t.table), params![key])?;
            }
            let placeholders: Vec<String> = (1..=t.columns.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "INSERT {} INTO {} ({}) VALUES ({})",
                if shared { "OR IGNORE" } else { "" },
                t.table,
                t.columns.join(", "),
                placeholders.join(", ")
            );
            for row in &t.rows {
                tx.execute(&sql, rusqlite::params_from_iter(row.iter()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn save_snapshot(
        &self,
        group_id: &[u8],
        epoch_after: u64,
        rank: &[u8; 32],
        ours: bool,
        rows: &[TableRows],
    ) -> Result<()> {
        let json = serde_json::to_string(rows).map_err(mls)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO omni_mls_snapshots (group_id, epoch, merged_rank, ours, rows)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![group_id, epoch_after as i64, rank.to_vec(), ours, json],
        )?;
        // The branch we are on, by the rank of the Commit merged at each epoch — what tells another
        // branch's Commit apart later, long after the snapshot window.
        let epoch = epoch_after.saturating_sub(1) as i64;
        self.conn.execute(
            "INSERT OR REPLACE INTO omni_mls_commit_log (group_id, epoch, rank) VALUES (?1, ?2, ?3)",
            params![group_id, epoch, rank.to_vec()],
        )?;
        self.conn.execute(
            "DELETE FROM omni_mls_commit_log WHERE group_id = ?1 AND epoch < ?2",
            params![group_id, epoch - COMMIT_LOG_EPOCHS],
        )?;
        Ok(())
    }

    /// The snapshot taken before the Commit that brought us to `epoch_after`.
    fn load_snapshot(&self, group_id: &[u8], epoch_after: u64) -> Result<Option<([u8; 32], bool, Vec<TableRows>)>> {
        let row: Option<(Vec<u8>, bool, String)> = self
            .conn
            .query_row(
                "SELECT merged_rank, ours, rows FROM omni_mls_snapshots WHERE group_id = ?1 AND epoch = ?2",
                params![group_id, epoch_after as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((rank, ours, json)) = row else {
            return Ok(None);
        };
        let rank: [u8; 32] = rank.try_into().map_err(|_| GroupError::Invalid("bad rank".into()))?;
        Ok(Some((rank, ours, serde_json::from_str(&json).map_err(mls)?)))
    }
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

/// Plain JSON codec — for tests and for callers that encrypt at another layer.
#[derive(Default)]
pub struct JsonCodec;

impl Codec for JsonCodec {
    type Error = serde_json::Error;
    fn to_vec<T: Serialize>(value: &T) -> std::result::Result<Vec<u8>, Self::Error> {
        serde_json::to_vec(value)
    }
    fn from_slice<T: serde::de::DeserializeOwned>(slice: &[u8]) -> std::result::Result<T, Self::Error> {
        serde_json::from_slice(slice)
    }
}

#[cfg(test)]
mod tests;
