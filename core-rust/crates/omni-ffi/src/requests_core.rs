use omni_identity::{AccountExportManifestV1, LocalProfileId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CreateAccountRequest {
    pub(crate) display_name: String,
    pub(crate) device_label: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UpdateLocalPublicProfileRequest {
    pub(crate) display_name: String,
    pub(crate) avatar_source_path: Option<String>,
    pub(crate) clear_avatar: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RestoreAccountFromExportManifestRequest {
    pub(crate) manifest: AccountExportManifestV1,
    pub(crate) mnemonic: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ExportCurrentDatabaseBackupRequest {
    pub(crate) destination_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InspectAccountBackupManifestRequest {
    pub(crate) backup_db_path: String,
    pub(crate) mnemonic: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnlockDatabaseWithMnemonicRequest {
    pub(crate) mnemonic: String,
}

/// `state`: `openable` — opens as it is; `needs_recovery_phrase` — encrypted under a key this machine
/// cannot unwrap (a folder from another machine or Windows account).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DatabaseLockStateResponse {
    pub(crate) state: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ImportDatabaseBackupRequest {
    pub(crate) backup_db_path: String,
    pub(crate) inspection_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RestoreAccountFromInputRequest {
    pub(crate) display_name: String,
    pub(crate) mnemonic: String,
    pub(crate) device_label: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InspectRecoveryInputRequest {
    pub(crate) mnemonic: String,
    pub(crate) display_name: Option<String>,
    pub(crate) device_label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LocalProfileRequest {
    pub(crate) profile_id: LocalProfileId,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnlockProfileWithPasswordRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnlockProfileWithMnemonicRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) mnemonic: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetLocalProfilePasswordRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChangeLocalProfilePasswordRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) current_password: String,
    pub(crate) new_password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RemoveLocalProfilePasswordRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) current_password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ConfirmDeleteLocalProfileRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) confirmation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ConfirmSignOutProfileRequest {
    pub(crate) profile_id: LocalProfileId,
    pub(crate) confirmation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ConfirmCurrentProfileActionRequest {
    pub(crate) confirmation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnlockCurrentProfileWithPasswordRequest {
    pub(crate) password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RevealCurrentProfileRecoveryPhraseWithPasswordRequest {
    pub(crate) password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UnlockCurrentProfileWithMnemonicRequest {
    pub(crate) mnemonic: String,
}
