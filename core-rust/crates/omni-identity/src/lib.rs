use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use omni_types::{OmniError, OmniResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod mnemonic;

pub const ACCOUNT_REPRESENTATION_VERSION_V2: u16 = 2;
pub const RECOVERY_REPRESENTATION_VERSION_V2: u16 = 2;
pub const ACCOUNT_ROOT_SECRET_BYTES: usize = mnemonic::ACCOUNT_ROOT_SECRET_BYTES;
pub const RECOVERY_PHRASE_V2_WORD_COUNT: usize = mnemonic::RECOVERY_PHRASE_V2_WORD_COUNT;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContactId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalProfileId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactRelationshipState {
    Pending,
    Approved,
    Declined,
}

impl ContactRelationshipState {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Declined => "declined",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "declined" => Ok(Self::Declined),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported contact relationship state '{}'",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProfileState {
    SignedIn,
    SignedOutLocked,
    Deleting,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLifecycleErrorCode {
    NoActiveProfile,
    ActiveProfileSwitchRequired,
    ProfileLocked,
    ProfileDeleting,
    ProfileDeleted,
    ProfileAlreadyPresent,
    UnsupportedLegacyLocalData,
    UnsupportedDatabaseFormat,
    UnsupportedLegacyProfileState,
    LifecycleTransitionNotAllowed,
    UnlockFailedInvalidPassword,
    UnlockFailedInvalidMnemonic,
    DangerousActionConfirmationRequired,
    RuntimeContextUnavailable,
}

impl ProfileLifecycleErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoActiveProfile => "no_active_profile",
            Self::ActiveProfileSwitchRequired => "active_profile_switch_required",
            Self::ProfileLocked => "profile_locked",
            Self::ProfileDeleting => "profile_deleting",
            Self::ProfileDeleted => "profile_deleted",
            Self::ProfileAlreadyPresent => "profile_already_present",
            Self::UnsupportedLegacyLocalData => "unsupported_legacy_local_data",
            Self::UnsupportedDatabaseFormat => "unsupported_database_format",
            Self::UnsupportedLegacyProfileState => "unsupported_legacy_profile_state",
            Self::LifecycleTransitionNotAllowed => "lifecycle_transition_not_allowed",
            Self::UnlockFailedInvalidPassword => "unlock_failed_invalid_password",
            Self::UnlockFailedInvalidMnemonic => "unlock_failed_invalid_mnemonic",
            Self::DangerousActionConfirmationRequired => "dangerous_action_confirmation_required",
            Self::RuntimeContextUnavailable => "runtime_context_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLifecycleDomainError {
    pub code: ProfileLifecycleErrorCode,
    pub message: String,
}

impl ProfileLifecycleDomainError {
    pub fn new(code: ProfileLifecycleErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn into_omni_error(self) -> OmniError {
        OmniError::InvalidInput(format!(
            "profile_lifecycle:{}:{}",
            self.code.as_str(),
            self.message
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileGateSummary {
    pub profile_id: LocalProfileId,
    pub state: LocalProfileState,
    pub is_unlocked: bool,
    pub requires_unlock: bool,
    pub destructive_transition_in_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLifecycleSummary {
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub state: LocalProfileState,
    pub is_unlocked: bool,
    pub requires_unlock: bool,
    pub messaging_allowed: bool,
    pub transport_allowed: bool,
    pub device_operations_allowed: bool,
    pub sync_operations_allowed: bool,
    pub destructive_action_allowed: bool,
    pub destructive_transition_in_progress: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFlowKind {
    UnlockSavedProfile,
    RestoreNewLocalProfile,
    ReopenExistingLocalProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMnemonicUsage {
    OwnershipProofForUnlock,
    RootBasisForRestore,
    RootBasisForReopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryConflictCode {
    ExistingProfileAlreadyPresent,
    ProfileLocked,
    ProfileDeleting,
    ProfileDeleted,
    InvalidMnemonic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryInputSummary {
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub flow_kind: RecoveryFlowKind,
    pub mnemonic_usage: RecoveryMnemonicUsage,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub device_label: Option<String>,
    #[serde(default)]
    pub active_profile_id: Option<LocalProfileId>,
    #[serde(default)]
    pub existing_profile: Option<LocalProfileSummary>,
    #[serde(default)]
    pub conflict: Option<RecoveryConflictCode>,
    pub can_unlock: bool,
    pub can_restore: bool,
    pub can_reopen: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountRootSecret(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPhraseV2 {
    pub version: u16,
    pub mnemonic: String,
}

impl RecoveryPhraseV2 {
    fn canonical(version: u16, mnemonic: String) -> Self {
        Self { version, mnemonic }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPublicIdentity {
    pub account_id: AccountId,
    pub display_name: String,
    pub verification_payload: String,
    pub safety_number_material: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLocalSecretState {
    pub profile_id: LocalProfileId,
    pub root_secret: AccountRootSecret,
    pub recovery_phrase_v2: RecoveryPhraseV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRepresentationV2 {
    pub version: u16,
    pub public_identity: AccountPublicIdentity,
    pub local_secret_state: AccountLocalSecretState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicAccountSummary {
    pub account_id: AccountId,
    pub display_name: String,
    pub verification_payload: String,
    pub safety_number_material: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccountProfileSummary {
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub display_name: String,
    pub account_representation_version: u16,
    #[serde(default = "default_local_profile_state")]
    pub state: LocalProfileState,
    pub has_root_secret: bool,
    pub has_recovery_phrase_v2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalProfileSummary {
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub display_name: String,
    pub state: LocalProfileState,
    pub account_representation_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProfileList {
    pub profiles: Vec<LocalProfileSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordProtectionSummary {
    pub profile_id: LocalProfileId,
    pub enabled: bool,
    pub algorithm: String,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
    pub protection_model: String,
    pub protection_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalProfileFootprintSummary {
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub state: LocalProfileState,
    pub contacts_count: usize,
    pub sessions_count: usize,
    pub messages_count: usize,
    pub has_password_protection: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DangerousActionKind {
    SignOutProfile,
    DeleteLocalProfile,
    RevokeCurrentDeviceAndWipe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DangerousActionPreview {
    pub profile_id: LocalProfileId,
    pub action: DangerousActionKind,
    pub title: String,
    pub summary: String,
    pub consequences: Vec<String>,
    pub requires_confirmation: bool,
    pub confirmation_token: Option<String>,
    pub supported_now: bool,
    pub unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBootstrapStateKind {
    NewDatabase,
    SupportedProfileDatabase,
    UnsupportedLegacyLocalData,
    UnsupportedDatabaseFormat,
    UnsupportedLegacyProfileState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBootstrapSummary {
    pub state: DatabaseBootstrapStateKind,
    pub supported: bool,
    pub reset_required: bool,
    pub can_create_new_account: bool,
    pub can_restore_from_mnemonic: bool,
    pub unsupported_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicAccountDescriptor {
    pub account_id: AccountId,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccountSnapshot {
    pub descriptor: PublicAccountDescriptor,
    /// `None` on a device that cannot open the root secret: a profile folder copied to another
    /// computer, where the DPAPI seal does not open. That device still acts for the account through
    /// its working key; the phrase is not there until it is entered.
    #[serde(default)]
    pub recovery_phrase_v2: Option<RecoveryPhraseV2>,
    pub current_device_id: DeviceId,
    pub restored_from_recovery: bool,
    #[serde(default = "default_account_representation_version")]
    pub account_representation_version: u16,
    #[serde(default)]
    pub public_summary: Option<PublicAccountSummary>,
    #[serde(default)]
    pub local_profile_summary: Option<LocalAccountProfileSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_hash_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_profile_updated_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountExportManifestV1 {
    pub version: u16,
    pub profile_id: LocalProfileId,
    pub account_id: AccountId,
    pub display_name: String,
    pub device_label: String,
    pub current_device_id: DeviceId,
    pub account_representation_version: u16,
    pub exported_at_unix_ms: i64,
    /// When this profile last saved a backup file (only in the current profile's manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDeviceSnapshot {
    pub device_id: DeviceId,
    pub account_id: AccountId,
    pub device_label: String,
    pub is_current: bool,
    pub restored_from_recovery: bool,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddContactInput {
    pub petname: String,
    pub remote_account_id: Option<AccountId>,
    pub public_display_name: Option<String>,
    pub remote_instance_id: Option<String>,
    pub primary_dialog_key: Option<String>,
    pub initial_observation: Option<ContactObservationSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ContactPresenceState {
    Online,
    Recently,
    #[default]
    Offline,
}


impl ContactPresenceState {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Recently => "recently",
            Self::Offline => "offline",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "online" => Ok(Self::Online),
            "recently" | "recent" => Ok(Self::Recently),
            "offline" => Ok(Self::Offline),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported contact presence state '{}'",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactRequestDirection {
    Incoming,
    Outgoing,
}

impl ContactRequestDirection {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "incoming" => Ok(Self::Incoming),
            "outgoing" => Ok(Self::Outgoing),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported contact request direction '{}'",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSnapshot {
    pub contact_id: ContactId,
    pub petname: String,
    pub remote_account_id: Option<AccountId>,
    pub public_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_hash_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_pending_hash_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_pending_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_avatar_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_profile_updated_at_unix_ms: Option<i64>,
    pub remote_instance_id: Option<String>,
    pub primary_dialog_key: Option<String>,
    pub relationship_state: ContactRelationshipState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_request_direction: Option<ContactRequestDirection>,
    #[serde(default)]
    pub has_met: bool,
    /// Friend-proxy (Axis 2): we consented to carry THIS contact's mesh traffic through our node.
    /// Off by default, revocable. See docs/decisions/friend-proxy-by-consent.md.
    #[serde(default)]
    pub friend_relay_consent: bool,
    pub is_blocked: bool,
    #[serde(default)]
    pub presence_state: ContactPresenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_last_seen_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_last_sent_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_last_activity_at_unix_ms: Option<i64>,
    pub removed_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactCardSnapshot {
    pub contact: ContactSnapshot,
    pub trust_state: TrustState,
    pub verification: VerificationSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatHeaderContactSummary {
    pub dialog_key: String,
    pub title: String,
    pub subtitle: String,
    pub contact_card: Option<ContactCardSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactChatTarget {
    pub contact_id: ContactId,
    pub dialog_key: String,
    pub title: String,
}

/// What we have seen of a contact's identity. Its devices are not part of it: each carries a
/// certificate chain from the account's master key, which is what vouches for it
/// (docs/bugs/safety-number-differs-per-own-device.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactObservationSnapshot {
    pub identity_key: Option<String>,
    pub observed_display_name: Option<String>,
    pub observed_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    ManualCompare,
    RecoveryAssist,
    ChallengeResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Unverified,
    Verified,
    ReverifyNeeded,
    IdentityChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSummary {
    pub last_verified_at_unix_ms: Option<i64>,
    pub verification_method: Option<VerificationMethod>,
    pub current_identity_key: Option<String>,
    pub verified_identity_key: Option<String>,
    /// The account's devices we know — certified by its master key and not revoked.
    pub device_count: usize,
    pub current_fingerprint: Option<String>,
    pub current_safety_number: Option<String>,
    pub verified_fingerprint: Option<String>,
    pub verified_safety_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// What a verification confirmed: the account's identity key. Its fingerprint and safety number
/// are a function of the key, so they are derived, not stored.
pub struct VerificationBaseline {
    pub verified_at_unix_ms: i64,
    pub verification_method: VerificationMethod,
    pub verified_identity_key: Option<String>,
}

/// The fingerprint and safety number of an account: of its identity key alone — the same on every
/// device of the account, and unchanged when a device is added or taken back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationMaterial {
    pub identity_key: Option<String>,
    pub fingerprint: Option<String>,
    pub safety_number: Option<String>,
}

fn default_account_representation_version() -> u16 {
    ACCOUNT_REPRESENTATION_VERSION_V2
}

fn default_local_profile_state() -> LocalProfileState {
    LocalProfileState::SignedIn
}

pub fn local_profile_summary_from_account(
    account: &LocalAccountSnapshot,
    state: LocalProfileState,
) -> Option<LocalProfileSummary> {
    let profile = account.local_profile_summary.as_ref()?;
    Some(LocalProfileSummary {
        profile_id: profile.profile_id.clone(),
        account_id: profile.account_id.clone(),
        display_name: profile.display_name.clone(),
        state,
        account_representation_version: profile.account_representation_version,
    })
}

pub fn build_saved_profile_list(profiles: Vec<LocalProfileSummary>) -> SavedProfileList {
    SavedProfileList { profiles }
}

pub fn build_account_export_manifest(
    account: &LocalAccountSnapshot,
    device: &LocalDeviceSnapshot,
    exported_at_unix_ms: i64,
) -> OmniResult<AccountExportManifestV1> {
    let profile = account.local_profile_summary.as_ref().ok_or_else(|| {
        OmniError::InvalidInput("local profile summary is missing for export".to_string())
    })?;

    Ok(AccountExportManifestV1 {
        version: 1,
        profile_id: profile.profile_id.clone(),
        account_id: account.descriptor.account_id.clone(),
        display_name: account.descriptor.display_name.clone(),
        device_label: device.device_label.clone(),
        current_device_id: account.current_device_id.clone(),
        account_representation_version: account.account_representation_version,
        exported_at_unix_ms,
        last_backup_at_unix_ms: None,
    })
}

pub fn build_profile_gate_summary(
    profile_id: &LocalProfileId,
    state: LocalProfileState,
) -> ProfileGateSummary {
    ProfileGateSummary {
        profile_id: profile_id.clone(),
        state,
        is_unlocked: state == LocalProfileState::SignedIn,
        requires_unlock: state == LocalProfileState::SignedOutLocked,
        destructive_transition_in_progress: state == LocalProfileState::Deleting,
    }
}

pub fn build_profile_lifecycle_summary(profile: &LocalProfileSummary) -> ProfileLifecycleSummary {
    let state = profile.state;
    let unlocked = state == LocalProfileState::SignedIn;
    let deleting = state == LocalProfileState::Deleting;
    let deleted = state == LocalProfileState::Deleted;
    let requires_unlock = state == LocalProfileState::SignedOutLocked;
    let can_operate = unlocked;
    let destructive_action_allowed = !deleting && !deleted;

    ProfileLifecycleSummary {
        profile_id: profile.profile_id.clone(),
        account_id: profile.account_id.clone(),
        state,
        is_unlocked: unlocked,
        requires_unlock,
        messaging_allowed: can_operate,
        transport_allowed: can_operate,
        device_operations_allowed: can_operate,
        sync_operations_allowed: can_operate,
        destructive_action_allowed,
        destructive_transition_in_progress: deleting,
    }
}

pub fn build_recovery_input_summary(
    profile_id: LocalProfileId,
    account_id: AccountId,
    active_profile_id: Option<&LocalProfileId>,
    existing_profile: Option<&LocalProfileSummary>,
    display_name: Option<String>,
    device_label: Option<String>,
) -> RecoveryInputSummary {
    let existing_profile = existing_profile.cloned();
    let (flow_kind, mnemonic_usage, conflict, can_unlock, can_restore, can_reopen, summary) =
        match existing_profile.as_ref().map(|profile| profile.state) {
            Some(LocalProfileState::SignedIn) => (
                RecoveryFlowKind::ReopenExistingLocalProfile,
                RecoveryMnemonicUsage::RootBasisForReopen,
                Some(RecoveryConflictCode::ExistingProfileAlreadyPresent),
                false,
                false,
                true,
                "This local profile is already known on this device. Reopen it from the profile chooser instead of restoring it from mnemonic.".to_string(),
            ),
            Some(LocalProfileState::SignedOutLocked) => (
                RecoveryFlowKind::UnlockSavedProfile,
                RecoveryMnemonicUsage::OwnershipProofForUnlock,
                Some(RecoveryConflictCode::ProfileLocked),
                true,
                false,
                false,
                "This saved local profile is locked on this device. Unlock it instead of restoring it from mnemonic.".to_string(),
            ),
            Some(LocalProfileState::Deleting) => (
                RecoveryFlowKind::ReopenExistingLocalProfile,
                RecoveryMnemonicUsage::RootBasisForReopen,
                Some(RecoveryConflictCode::ProfileDeleting),
                false,
                false,
                false,
                "This local profile is currently deleting and cannot be recovered right now.".to_string(),
            ),
            Some(LocalProfileState::Deleted) => (
                RecoveryFlowKind::RestoreNewLocalProfile,
                RecoveryMnemonicUsage::RootBasisForRestore,
                Some(RecoveryConflictCode::ProfileDeleted),
                false,
                false,
                false,
                "This local profile was removed from the device. A mnemonic can create a new local profile only if it does not already exist locally.".to_string(),
            ),
            None => (
                RecoveryFlowKind::RestoreNewLocalProfile,
                RecoveryMnemonicUsage::RootBasisForRestore,
                None,
                false,
                true,
                false,
                "This mnemonic can create a new local profile on this device.".to_string(),
            ),
        };

    let mut summary_lines = vec![summary];
    summary_lines.push(format!("Profile ID: {}", profile_id.0));
    summary_lines.push(format!("Account ID: {}", account_id.0));
    if let Some(active_profile_id) = active_profile_id {
        summary_lines.push(format!("Active profile: {}", active_profile_id.0));
    }
    if let Some(display_name) = display_name
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        summary_lines.push(format!("Display name: {display_name}"));
    }
    if let Some(device_label) = device_label
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        summary_lines.push(format!("Device label: {device_label}"));
    }

    RecoveryInputSummary {
        profile_id,
        account_id,
        flow_kind,
        mnemonic_usage,
        display_name,
        device_label,
        active_profile_id: active_profile_id.cloned(),
        existing_profile,
        conflict,
        can_unlock,
        can_restore,
        can_reopen,
        summary: summary_lines.join("\n"),
    }
}

pub fn create_account_identity_v2(
    display_name: &str,
    device_label: &str,
    now_unix_ms: i64,
) -> OmniResult<(
    LocalAccountSnapshot,
    LocalDeviceSnapshot,
    AccountRepresentationV2,
)> {
    let root_secret = generate_random_account_root();

    create_account_identity_v2_from_root_secret(
        &root_secret,
        display_name,
        device_label,
        now_unix_ms,
    )
}

pub fn create_account_identity_v2_from_root_secret(
    root_secret: &AccountRootSecret,
    display_name: &str,
    device_label: &str,
    now_unix_ms: i64,
) -> OmniResult<(
    LocalAccountSnapshot,
    LocalDeviceSnapshot,
    AccountRepresentationV2,
)> {
    let display_name = clamp_display_name(&validate_non_empty("display_name", display_name)?);
    let device_label = clamp_display_name(&validate_non_empty("device_label", device_label)?);
    let public_identity = derive_account_public_identity(root_secret, &display_name)?;
    let account_id = public_identity.account_id.clone();
    let device_id = DeviceId(next_identifier("dev"));
    let recovery_v2 = derive_recovery_phrase_v2(root_secret)?;
    let local_secret_state = AccountLocalSecretState {
        profile_id: derive_local_profile_id_from_root(root_secret, &account_id)?,
        root_secret: root_secret.clone(),
        recovery_phrase_v2: recovery_v2.clone(),
    };
    let representation_v2 = AccountRepresentationV2 {
        version: ACCOUNT_REPRESENTATION_VERSION_V2,
        public_identity: public_identity.clone(),
        local_secret_state: local_secret_state.clone(),
    };
    let public_summary = PublicAccountSummary {
        account_id: public_identity.account_id.clone(),
        display_name: public_identity.display_name.clone(),
        verification_payload: public_identity.verification_payload.clone(),
        safety_number_material: public_identity.safety_number_material.clone(),
    };
    let local_profile_summary = LocalAccountProfileSummary {
        profile_id: local_secret_state.profile_id,
        account_id: account_id.clone(),
        display_name: display_name.clone(),
        account_representation_version: ACCOUNT_REPRESENTATION_VERSION_V2,
        state: LocalProfileState::SignedIn,
        has_root_secret: true,
        has_recovery_phrase_v2: true,
    };
    let (account, device) = build_account_snapshots(
        account_id,
        device_id,
        display_name,
        device_label,
        recovery_v2,
        false,
        ACCOUNT_REPRESENTATION_VERSION_V2,
        Some(public_summary),
        Some(local_profile_summary),
        now_unix_ms,
    );

    Ok((account, device, representation_v2))
}

pub fn restore_account_identity_v2(
    recovery_phrase_v2: &RecoveryPhraseV2,
    display_name: &str,
    device_label: &str,
    now_unix_ms: i64,
) -> OmniResult<(
    LocalAccountSnapshot,
    LocalDeviceSnapshot,
    AccountRepresentationV2,
)> {
    validate_recovery_phrase_v2(recovery_phrase_v2)?;
    let root_secret = account_root_secret_from_recovery_phrase_v2(recovery_phrase_v2)?;
    let display_name = validate_non_empty("display_name", display_name)?;
    create_account_identity_v2_from_root_secret(
        &root_secret,
        &display_name,
        device_label,
        now_unix_ms,
    )
    .map(|(mut account, mut device, representation_v2)| {
        account.restored_from_recovery = true;
        device.restored_from_recovery = true;
        (account, device, representation_v2)
    })
}

pub fn validate_recovery_phrase_v2(recovery: &RecoveryPhraseV2) -> OmniResult<()> {
    if recovery.version != RECOVERY_REPRESENTATION_VERSION_V2 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported recovery representation version '{}'",
            recovery.version
        )));
    }

    let normalized_mnemonic = normalize_mnemonic_v2(&recovery.mnemonic)?;
    let _root_secret = decode_mnemonic_v2_to_root(&normalized_mnemonic)?;

    Ok(())
}

/// The account MASTER key seed (Ed25519), derived from the root secret — the top of the
/// account's key chain (docs/decisions/multi-device-equal-nodes.md). It never leaves the device
/// that holds the root secret; what travels is its public half, from which the account id and
/// the Omni-ID are computed, so anyone can check that a key belongs to an account.
pub fn derive_account_master_seed(root_secret: &AccountRootSecret) -> OmniResult<[u8; 32]> {
    let root_bytes = parse_root_secret(root_secret)?;
    let mut seed = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, &root_bytes)
        .expand(b"omni-account-master-v1", &mut seed)
        .map_err(|_| OmniError::Internal("hkdf expand failed".to_string()))?;
    Ok(seed)
}

pub fn derive_account_master_public_key(root_secret: &AccountRootSecret) -> OmniResult<[u8; 32]> {
    let seed = derive_account_master_seed(root_secret)?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes())
}

/// The account id is a hash of the PUBLIC master key — verifiable by anyone holding that key,
/// unlike the old hash-of-secret label.
pub fn account_id_from_master_public_key(master_public_key: &[u8; 32]) -> AccountId {
    let mut payload = b"omni-account-id-v3|".to_vec();
    payload.extend_from_slice(master_public_key);
    AccountId(format!("acct-{}", hex::encode(&sha256(&payload)[..16])))
}

pub fn derive_account_id_from_root(root_secret: &AccountRootSecret) -> OmniResult<AccountId> {
    Ok(account_id_from_master_public_key(
        &derive_account_master_public_key(root_secret)?,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedMnemonicV2 {
    pub normalized_mnemonic: String,
    pub root_secret: AccountRootSecret,
    pub account_id: AccountId,
}

pub fn generate_random_account_root() -> AccountRootSecret {
    AccountRootSecret(hex::encode(mnemonic::generate_random_account_root_bytes()))
}

pub fn encode_root_to_mnemonic_v2(root_secret: &AccountRootSecret) -> OmniResult<String> {
    let root_bytes = parse_root_secret(root_secret)?;
    mnemonic::encode_root_to_mnemonic_v2_bytes(&root_bytes)
}

pub fn decode_mnemonic_v2_to_root(mnemonic: &str) -> OmniResult<AccountRootSecret> {
    let root_bytes = mnemonic::decode_mnemonic_v2_to_root_bytes(mnemonic)?;
    Ok(AccountRootSecret(hex::encode(root_bytes)))
}

pub fn validate_mnemonic_v2(mnemonic: &str) -> OmniResult<()> {
    mnemonic::validate_mnemonic_v2(mnemonic)
}

pub fn normalize_mnemonic_v2(mnemonic: &str) -> OmniResult<String> {
    mnemonic::normalize_mnemonic_v2(mnemonic)
}

pub fn parse_mnemonic_v2(mnemonic: &str) -> OmniResult<ParsedMnemonicV2> {
    let parsed = mnemonic::parse_mnemonic_v2(mnemonic)?;
    let root_secret = AccountRootSecret(hex::encode(parsed.root_bytes));
    let account_id = derive_account_id_from_root(&root_secret)?;

    Ok(ParsedMnemonicV2 {
        normalized_mnemonic: parsed.normalized_mnemonic,
        root_secret,
        account_id,
    })
}

pub fn parse_recovery_input(recovery_input: &str) -> OmniResult<ParsedMnemonicV2> {
    let normalized = recovery_input.trim();
    if normalized.is_empty() {
        return Err(OmniError::InvalidInput(
            "recovery input must not be empty".to_string(),
        ));
    }

    parse_mnemonic_v2(normalized)
}

pub fn build_recovery_phrase_v2_from_input(recovery_input: &str) -> OmniResult<RecoveryPhraseV2> {
    let parsed_mnemonic = parse_recovery_input(recovery_input)?;
    Ok(RecoveryPhraseV2::canonical(
        RECOVERY_REPRESENTATION_VERSION_V2,
        parsed_mnemonic.normalized_mnemonic,
    ))
}

pub fn derive_recovery_phrase_v2(root_secret: &AccountRootSecret) -> OmniResult<RecoveryPhraseV2> {
    let mnemonic = encode_root_to_mnemonic_v2(root_secret)?;
    Ok(RecoveryPhraseV2::canonical(
        RECOVERY_REPRESENTATION_VERSION_V2,
        mnemonic,
    ))
}

pub fn account_root_secret_from_recovery_phrase_v2(
    recovery: &RecoveryPhraseV2,
) -> OmniResult<AccountRootSecret> {
    decode_mnemonic_v2_to_root(&recovery.mnemonic)
}

pub fn account_root_secret_from_mnemonic_phrase_v2(
    mnemonic: &str,
) -> OmniResult<AccountRootSecret> {
    decode_mnemonic_v2_to_root(mnemonic)
}

pub fn derive_account_public_identity(
    root_secret: &AccountRootSecret,
    display_name: &str,
) -> OmniResult<AccountPublicIdentity> {
    account_public_identity_from_master_public_key(
        &derive_account_master_public_key(root_secret)?,
        display_name,
    )
}

/// The account's public identity from its PUBLIC master key — what every device of the account
/// holds, a copied folder on another computer included, which cannot open the DPAPI-sealed root
/// secret (docs/decisions/multi-device-equal-nodes.md).
pub fn account_public_identity_from_master_public_key(
    master_public_key: &[u8; 32],
    display_name: &str,
) -> OmniResult<AccountPublicIdentity> {
    let display_name = validate_non_empty("display_name", display_name)?;
    let account_id = account_id_from_master_public_key(master_public_key);

    let mut verification_payload_bytes = b"omni-account-verification-v3|".to_vec();
    verification_payload_bytes.extend_from_slice(master_public_key);
    verification_payload_bytes.extend_from_slice(display_name.as_bytes());
    let verification_payload = hex::encode(sha256(&verification_payload_bytes));

    Ok(AccountPublicIdentity {
        account_id,
        display_name,
        safety_number_material: fingerprint_to_safety_number(&verification_payload),
        verification_payload,
    })
}

pub fn create_contact_id() -> ContactId {
    ContactId(next_identifier("contact"))
}

pub fn validate_add_contact_input(input: &AddContactInput) -> OmniResult<()> {
    if input.petname.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "petname must not be empty".to_string(),
        ));
    }

    if let Some(remote_account_id) = &input.remote_account_id {
        if remote_account_id.0.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "remote_account_id must not be empty when present".to_string(),
            ));
        }
    }

    if let Some(remote_instance_id) = &input.remote_instance_id {
        if remote_instance_id.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "remote_instance_id must not be empty when present".to_string(),
            ));
        }
    }

    if let Some(primary_dialog_key) = &input.primary_dialog_key {
        if primary_dialog_key.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "primary_dialog_key must not be empty when present".to_string(),
            ));
        }
    }

    if let Some(observation) = &input.initial_observation {
        validate_contact_observation(observation)?;
    }

    Ok(())
}

pub fn validate_contact_observation(observation: &ContactObservationSnapshot) -> OmniResult<()> {
    if let Some(identity_key) = &observation.identity_key {
        if identity_key.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "identity observation key must not be empty".to_string(),
            ));
        }
    }

    Ok(())
}

// Snapshot constructor: the many args ARE the record's fields — a params struct would only relocate
// them to the call site, not remove the essential count. Conscious allow, not a smell to refactor.
#[allow(clippy::too_many_arguments)]
pub fn build_contact_snapshot(
    contact_id: ContactId,
    petname: String,
    remote_account_id: Option<AccountId>,
    public_display_name: Option<String>,
    public_avatar_hash_hex: Option<String>,
    public_avatar_mime_type: Option<String>,
    public_avatar_pending_hash_hex: Option<String>,
    public_avatar_pending_mime_type: Option<String>,
    public_avatar_image_path: Option<String>,
    public_profile_updated_at_unix_ms: Option<i64>,
    remote_instance_id: Option<String>,
    primary_dialog_key: Option<String>,
    relationship_state: ContactRelationshipState,
    pending_request_direction: Option<ContactRequestDirection>,
    has_met: bool,
    friend_relay_consent: bool,
    presence_state: ContactPresenceState,
    presence_last_seen_at_unix_ms: Option<i64>,
    presence_last_sent_at_unix_ms: Option<i64>,
    presence_last_activity_at_unix_ms: Option<i64>,
    removed_at_unix_ms: Option<i64>,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
) -> ContactSnapshot {
    let is_blocked = matches!(relationship_state, ContactRelationshipState::Declined);
    let pending_request_direction =
        if matches!(relationship_state, ContactRelationshipState::Pending) {
            pending_request_direction
        } else {
            None
        };
    ContactSnapshot {
        contact_id,
        petname,
        remote_account_id: remote_account_id.filter(|value| !value.0.trim().is_empty()),
        public_display_name: public_display_name.filter(|value| !value.trim().is_empty()),
        public_avatar_hash_hex: public_avatar_hash_hex.filter(|value| !value.trim().is_empty()),
        public_avatar_mime_type: public_avatar_mime_type.filter(|value| !value.trim().is_empty()),
        public_avatar_pending_hash_hex: public_avatar_pending_hash_hex
            .filter(|value| !value.trim().is_empty()),
        public_avatar_pending_mime_type: public_avatar_pending_mime_type
            .filter(|value| !value.trim().is_empty()),
        public_avatar_image_path: public_avatar_image_path.filter(|value| !value.trim().is_empty()),
        public_profile_updated_at_unix_ms,
        remote_instance_id: remote_instance_id.filter(|value| !value.trim().is_empty()),
        primary_dialog_key: primary_dialog_key.filter(|value| !value.trim().is_empty()),
        relationship_state,
        pending_request_direction,
        has_met,
        friend_relay_consent,
        is_blocked,
        presence_state,
        presence_last_seen_at_unix_ms,
        presence_last_sent_at_unix_ms,
        presence_last_activity_at_unix_ms,
        removed_at_unix_ms,
        created_at_unix_ms,
        updated_at_unix_ms,
    }
}

pub fn build_verification_baseline(
    observation: &ContactObservationSnapshot,
    method: VerificationMethod,
    verified_at_unix_ms: i64,
) -> OmniResult<VerificationBaseline> {
    validate_contact_observation(observation)?;
    Ok(VerificationBaseline {
        verified_at_unix_ms,
        verification_method: method,
        verified_identity_key: observation.identity_key.clone(),
    })
}

pub fn derive_verification_material(
    observation: &ContactObservationSnapshot,
) -> OmniResult<VerificationMaterial> {
    validate_contact_observation(observation)?;
    Ok(verification_material_for_identity_key(observation.identity_key.as_deref()))
}

/// The fingerprint and safety number of an identity key (see [`VerificationMaterial`]).
pub fn verification_material_for_identity_key(identity_key: Option<&str>) -> VerificationMaterial {
    let fingerprint = compute_fingerprint(identity_key);
    VerificationMaterial {
        identity_key: identity_key.map(str::to_string),
        safety_number: fingerprint.as_deref().map(fingerprint_to_safety_number),
        fingerprint,
    }
}

/// `device_count`: the contact's devices we know (certified, not revoked) — shown, not judged: a
/// device the account's master key vouches for changes nothing about the trust.
pub fn evaluate_trust(
    observation: Option<&ContactObservationSnapshot>,
    verification: Option<&VerificationBaseline>,
    device_count: usize,
) -> (TrustState, VerificationSummary) {
    let current_material = observation
        .map(derive_verification_material)
        .transpose()
        .unwrap_or(None);
    let current_identity_key = current_material
        .as_ref()
        .and_then(|value| value.identity_key.clone());
    let current_fingerprint = current_material
        .as_ref()
        .and_then(|value| value.fingerprint.clone());
    let current_safety_number = current_material
        .as_ref()
        .and_then(|value| value.safety_number.clone());

    let Some(verification) = verification else {
        return (
            TrustState::Unverified,
            VerificationSummary {
                last_verified_at_unix_ms: None,
                verification_method: None,
                current_identity_key,
                verified_identity_key: None,
                device_count,
                current_fingerprint,
                current_safety_number,
                verified_fingerprint: None,
                verified_safety_number: None,
            },
        );
    };

    let trust_state = match current_identity_key.as_deref() {
        None => TrustState::ReverifyNeeded,
        Some(current_identity_key) => {
            if verification.verified_identity_key.as_deref() != Some(current_identity_key) {
                TrustState::IdentityChanged
            } else {
                TrustState::Verified
            }
        }
    };
    let verified_material = verification_material_for_identity_key(verification.verified_identity_key.as_deref());

    (
        trust_state,
        VerificationSummary {
            last_verified_at_unix_ms: Some(verification.verified_at_unix_ms),
            verification_method: Some(verification.verification_method),
            current_identity_key,
            verified_identity_key: verification.verified_identity_key.clone(),
            device_count,
            current_fingerprint,
            current_safety_number,
            verified_fingerprint: verified_material.fingerprint,
            verified_safety_number: verified_material.safety_number,
        },
    )
}

// Snapshot constructor — args are the record's fields (see `build_contact_snapshot`).
#[allow(clippy::too_many_arguments)]
fn build_account_snapshots(
    account_id: AccountId,
    device_id: DeviceId,
    display_name: String,
    device_label: String,
    recovery_phrase_v2: RecoveryPhraseV2,
    restored_from_recovery: bool,
    account_representation_version: u16,
    public_summary: Option<PublicAccountSummary>,
    local_profile_summary: Option<LocalAccountProfileSummary>,
    now_unix_ms: i64,
) -> (LocalAccountSnapshot, LocalDeviceSnapshot) {
    let descriptor = PublicAccountDescriptor {
        account_id: account_id.clone(),
        display_name,
    };
    let account = LocalAccountSnapshot {
        descriptor: descriptor.clone(),
        recovery_phrase_v2: Some(recovery_phrase_v2),
        current_device_id: device_id.clone(),
        restored_from_recovery,
        account_representation_version,
        public_summary,
        local_profile_summary,
        public_avatar_hash_hex: None,
        public_avatar_mime_type: None,
        public_avatar_image_path: None,
        public_profile_updated_at_unix_ms: Some(now_unix_ms),
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };
    let device = LocalDeviceSnapshot {
        device_id,
        account_id,
        device_label,
        is_current: true,
        restored_from_recovery,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };

    (account, device)
}

pub fn derive_local_profile_id_from_root(
    root_secret: &AccountRootSecret,
    account_id: &AccountId,
) -> OmniResult<LocalProfileId> {
    let root_bytes = parse_root_secret(root_secret)?;
    let mut payload = b"omni-local-profile-v2|".to_vec();
    payload.extend_from_slice(account_id.0.as_bytes());
    payload.extend_from_slice(&root_bytes);
    Ok(LocalProfileId(format!(
        "profile-{}",
        hex::encode(&sha256(&payload)[..12])
    )))
}

fn parse_root_secret(
    root_secret: &AccountRootSecret,
) -> OmniResult<[u8; ACCOUNT_ROOT_SECRET_BYTES]> {
    let normalized = root_secret.0.trim();
    if normalized.is_empty() {
        return Err(OmniError::InvalidInput(
            "account root secret must not be empty".to_string(),
        ));
    }

    let decoded = hex::decode(normalized).map_err(|error| {
        OmniError::InvalidInput(format!("invalid account root secret hex: {error}"))
    })?;
    if decoded.len() != ACCOUNT_ROOT_SECRET_BYTES {
        return Err(OmniError::InvalidInput(format!(
            "account root secret must have {ACCOUNT_ROOT_SECRET_BYTES} bytes"
        )));
    }
    let mut bytes = [0u8; ACCOUNT_ROOT_SECRET_BYTES];
    bytes.copy_from_slice(&decoded);
    Ok(bytes)
}

/// The longest display name (or device label) kept, in characters. Nothing bounded it: a 5000-char
/// name ran off the profile screen and every contact's list it reached (QA 2026-10-17). The shell's
/// fields stop at it; a longer one — a peer's profile, a script — is cut to it here.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// `value` trimmed and cut to `MAX_DISPLAY_NAME_CHARS` characters.
pub fn clamp_display_name(value: &str) -> String {
    value.trim().chars().take(MAX_DISPLAY_NAME_CHARS).collect::<String>().trim_end().to_string()
}

fn validate_non_empty(field_name: &str, value: &str) -> OmniResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must not be empty"
        )));
    }

    Ok(trimmed.to_string())
}

/// The account id is the hash of the account's master key (`derive_account_id_from_root`), which
/// certifies every device of the account — so the fingerprint is of it alone.
fn compute_fingerprint(identity_key: Option<&str>) -> Option<String> {
    let identity_key = identity_key?.trim();
    if identity_key.is_empty() {
        return None;
    }
    let mut payload = b"omni-safety-number-v2|".to_vec();
    payload.extend_from_slice(identity_key.as_bytes());
    Some(hex::encode(sha256(&payload)))
}

fn fingerprint_to_safety_number(fingerprint: &str) -> String {
    let numeric = fingerprint
        .chars()
        .filter_map(|character| character.to_digit(16))
        .fold(String::new(), |mut acc, value| {
            acc.push(char::from(b'0' + (value % 10) as u8));
            acc
        });

    let digits = if numeric.len() >= 12 {
        numeric[..12].to_string()
    } else {
        format!("{numeric:0<12}")
    };

    format!("{} {} {}", &digits[0..4], &digits[4..8], &digits[8..12])
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut result = [0u8; 32];
    result.copy_from_slice(&digest);
    result
}

/// A fresh id for a device of the account — a new installation, or a copied folder that wakes up
/// somewhere else (docs/decisions/multi-device-equal-nodes.md).
pub fn new_device_id() -> DeviceId {
    DeviceId(next_identifier("dev"))
}

fn next_identifier(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        current_unix_ms(),
        ID_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::{
        account_root_secret_from_recovery_phrase_v2, build_recovery_input_summary,
        build_recovery_phrase_v2_from_input, build_verification_baseline,
        create_account_identity_v2, create_account_identity_v2_from_root_secret,
        derive_account_id_from_root, derive_account_public_identity, derive_recovery_phrase_v2,
        derive_verification_material, evaluate_trust, normalize_mnemonic_v2, parse_recovery_input,
        restore_account_identity_v2, validate_mnemonic_v2, validate_recovery_phrase_v2, AccountId,
        AccountRootSecret, AddContactInput, ContactObservationSnapshot,
        LocalProfileId, LocalProfileState, LocalProfileSummary, RecoveryConflictCode,
        RecoveryFlowKind, RecoveryMnemonicUsage, TrustState, VerificationMethod,
        ACCOUNT_REPRESENTATION_VERSION_V2, RECOVERY_PHRASE_V2_WORD_COUNT,
        RECOVERY_REPRESENTATION_VERSION_V2,
    };

    #[test]
    fn a_display_name_is_cut_to_the_limit_by_characters_not_bytes() {
        use super::{clamp_display_name, MAX_DISPLAY_NAME_CHARS};
        assert_eq!(clamp_display_name("  Алексей  "), "Алексей");
        let long = "Я".repeat(5000);
        let cut = clamp_display_name(&long);
        assert_eq!(cut.chars().count(), MAX_DISPLAY_NAME_CHARS, "Cyrillic is cut on a character");
        let spaced = format!("{} хвост", "а".repeat(MAX_DISPLAY_NAME_CHARS - 1));
        assert!(!clamp_display_name(&spaced).ends_with(' '), "no trailing space left by the cut");
    }

    #[test]
    fn create_account_identity_v2_produces_canonical_24_word_mnemonic() {
        let (account, device, representation_v2) =
            create_account_identity_v2("Alice", "Device", 123).unwrap();
        assert_eq!(account.current_device_id, device.device_id);
        assert_eq!(
            account.account_representation_version,
            ACCOUNT_REPRESENTATION_VERSION_V2
        );
        assert_eq!(
            account
                .recovery_phrase_v2
                .as_ref()
                .unwrap()
                .mnemonic
                .split_whitespace()
                .count(),
            RECOVERY_PHRASE_V2_WORD_COUNT
        );
        assert_eq!(
            account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
            representation_v2
                .local_secret_state
                .recovery_phrase_v2
                .mnemonic
        );
        validate_mnemonic_v2(&account.recovery_phrase_v2.as_ref().unwrap().mnemonic).unwrap();
    }

    #[test]
    fn recovery_input_summary_distinguishes_locked_profile_from_new_restore() {
        let profile_id = LocalProfileId("profile-locked".to_string());
        let account_id = AccountId("account-locked".to_string());
        let existing_profile = LocalProfileSummary {
            profile_id: profile_id.clone(),
            account_id: account_id.clone(),
            display_name: "Alice".to_string(),
            state: LocalProfileState::SignedOutLocked,
            account_representation_version: RECOVERY_REPRESENTATION_VERSION_V2,
        };

        let summary = build_recovery_input_summary(
            profile_id.clone(),
            account_id.clone(),
            None,
            Some(&existing_profile),
            Some("Alice".to_string()),
            Some("Laptop".to_string()),
        );

        assert_eq!(summary.flow_kind, RecoveryFlowKind::UnlockSavedProfile);
        assert_eq!(
            summary.mnemonic_usage,
            RecoveryMnemonicUsage::OwnershipProofForUnlock
        );
        assert_eq!(summary.conflict, Some(RecoveryConflictCode::ProfileLocked));
        assert!(summary.can_unlock);
        assert!(!summary.can_restore);
        assert!(!summary.can_reopen);
    }

    #[test]
    fn recovery_input_summary_marks_new_profile_restore_as_restorable() {
        let profile_id = LocalProfileId("profile-new".to_string());
        let account_id = AccountId("account-new".to_string());

        let summary = build_recovery_input_summary(
            profile_id.clone(),
            account_id.clone(),
            None,
            None,
            Some("Alice".to_string()),
            Some("Laptop".to_string()),
        );

        assert_eq!(summary.flow_kind, RecoveryFlowKind::RestoreNewLocalProfile);
        assert_eq!(
            summary.mnemonic_usage,
            RecoveryMnemonicUsage::RootBasisForRestore
        );
        assert_eq!(summary.conflict, None);
        assert!(!summary.can_unlock);
        assert!(summary.can_restore);
        assert!(!summary.can_reopen);
    }

    #[test]
    fn recovery_phrase_v2_round_trip_works() {
        let root = AccountRootSecret(
            "ffffffff00000000aaaaaaaa5555555511111111222222223333333344444444".to_string(),
        );
        let phrase = derive_recovery_phrase_v2(&root).unwrap();
        validate_recovery_phrase_v2(&phrase).unwrap();
        let parsed = account_root_secret_from_recovery_phrase_v2(&phrase).unwrap();
        assert_eq!(parsed, root);
    }

    #[test]
    fn mnemonic_v2_normalization_and_checksum_validation_work() {
        let root = AccountRootSecret(
            "ababababababababababababababababcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".to_string(),
        );
        let phrase = derive_recovery_phrase_v2(&root).unwrap();
        let uppercase = phrase.mnemonic.to_uppercase();
        let normalized = normalize_mnemonic_v2(&uppercase).unwrap();
        assert_eq!(normalized, phrase.mnemonic);

        let mut words = normalized
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        words[0] = "zoo".to_string();
        let invalid_checksum = words.join(" ");
        let error = validate_mnemonic_v2(&invalid_checksum).unwrap_err();
        assert!(error
            .to_string()
            .contains("checksum or word format check failed"));
    }

    #[test]
    fn deterministic_account_identity_derivation_depends_on_root_secret() {
        let root_a = AccountRootSecret(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
        );
        let root_b = AccountRootSecret(
            "ffffffff00000000aaaaaaaa5555555511111111222222223333333344444444".to_string(),
        );

        let a1 = derive_account_id_from_root(&root_a).unwrap();
        let a2 = derive_account_id_from_root(&root_a).unwrap();
        let b = derive_account_id_from_root(&root_b).unwrap();

        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn parse_recovery_input_accepts_only_mnemonic_v2() {
        let root = AccountRootSecret(
            "ababababababababababababababababcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".to_string(),
        );
        let mnemonic = derive_recovery_phrase_v2(&root).unwrap().mnemonic;
        let parsed = parse_recovery_input(&mnemonic).unwrap();
        assert_eq!(parsed.normalized_mnemonic, mnemonic);

        let error = parse_recovery_input("recovery-legacy-code").unwrap_err();
        assert!(error.to_string().contains("invalid"));
    }

    #[test]
    fn build_recovery_phrase_from_input_produces_canonical_v2() {
        let root = AccountRootSecret(
            "12341234123412341234123412341234abababababababababababababababab".to_string(),
        );
        let mnemonic = derive_recovery_phrase_v2(&root).unwrap().mnemonic;
        let built = build_recovery_phrase_v2_from_input(&mnemonic).unwrap();
        assert_eq!(built.version, RECOVERY_REPRESENTATION_VERSION_V2);
        assert_eq!(built.mnemonic, mnemonic);
    }

    #[test]
    fn restore_account_identity_v2_reuses_account_and_marks_restored() {
        let root = AccountRootSecret(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa11111111111111111111111111111111".to_string(),
        );
        let (created, _, _) =
            create_account_identity_v2_from_root_secret(&root, "Alice", "Device-A", 100).unwrap();
        let recovery = derive_recovery_phrase_v2(&root).unwrap();

        let (restored, restored_device, restored_v2) =
            restore_account_identity_v2(&recovery, "Alice", "Device-B", 200).unwrap();

        assert_eq!(
            restored.descriptor.account_id,
            created.descriptor.account_id
        );
        assert_ne!(restored.current_device_id, created.current_device_id);
        assert!(restored.restored_from_recovery);
        assert!(restored_device.restored_from_recovery);
        assert_eq!(
            restored_v2.public_identity.account_id,
            restored.descriptor.account_id
        );
    }

    #[test]
    fn create_account_identity_v2_derives_deterministic_public_identity() {
        let root = AccountRootSecret(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
        );
        let first = derive_account_public_identity(&root, "Alice").unwrap();
        let second = derive_account_public_identity(&root, "Alice").unwrap();
        assert_eq!(first, second);

        let (account, _, _) =
            create_account_identity_v2_from_root_secret(&root, "Alice", "Device", 100).unwrap();
        assert_eq!(account.descriptor.account_id, first.account_id);
    }

    /// The trust follows the identity key alone: another device of the account changes nothing, a
    /// different key is a different identity, and the verified code is the current one.
    #[test]
    fn trust_follows_the_identity_key_alone() {
        let observation = |key: &str| ContactObservationSnapshot {
            identity_key: Some(key.to_string()),
            observed_display_name: Some("Alice".to_string()),
            observed_at_unix_ms: 100,
        };
        let baseline =
            build_verification_baseline(&observation("identity-a"), VerificationMethod::ManualCompare, 100)
                .unwrap();

        let (trust_state, summary) = evaluate_trust(Some(&observation("identity-a")), Some(&baseline), 3);
        assert_eq!(trust_state, TrustState::Verified);
        assert_eq!(summary.device_count, 3);
        assert_eq!(summary.verified_safety_number, summary.current_safety_number);
        let (trust_state, _) = evaluate_trust(Some(&observation("identity-a")), Some(&baseline), 1);
        assert_eq!(trust_state, TrustState::Verified, "a device more or less is not a reason to re-verify");
        let (trust_state, _) = evaluate_trust(Some(&observation("identity-b")), Some(&baseline), 1);
        assert_eq!(trust_state, TrustState::IdentityChanged);
    }

    #[test]
    fn add_contact_input_requires_non_empty_petname() {
        let input = AddContactInput {
            petname: "   ".to_string(),
            remote_account_id: None,
            public_display_name: None,
            remote_instance_id: None,
            primary_dialog_key: None,
            initial_observation: None,
        };

        let error = super::validate_add_contact_input(&input).unwrap_err();
        assert!(error.to_string().contains("petname"));
    }

    #[test]
    fn verification_material_derives_fingerprint_and_safety_number() {
        let material = derive_verification_material(&ContactObservationSnapshot {
            identity_key: Some("acct-1".to_string()),
            observed_display_name: Some("Alice".to_string()),
            observed_at_unix_ms: 100,
        })
        .unwrap();

        assert!(material.fingerprint.is_some());
        assert!(material.safety_number.is_some());
        assert_eq!(material, super::verification_material_for_identity_key(Some("acct-1")));
    }
}
