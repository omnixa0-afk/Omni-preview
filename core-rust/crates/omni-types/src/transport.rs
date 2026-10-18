
use serde::{Deserialize, Serialize};

use crate::{
    OmniError, OmniResult, DIALOG_KEY_ACCOUNT_PREFIX,
    DIALOG_KEY_INSTANCE_PREFIX,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Idle,
    Listening,
    Connected,
    ListeningAndConnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TransportMode {
    #[default]
    Quic,
}


impl TransportMode {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Quic => "quic",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "quic" => Ok(Self::Quic),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported transport mode '{}'",
                other
            ))),
        }
    }
}

/// The carrier of OUR hop — the link from this node to the next one, whether that is a relay or the
/// contact itself (owner, 2026-10-11: «A auto/tcp/quic B»). What the next node does onwards is its
/// own policy. Surfaced in the GUI as «Авто / TCP / QUIC». See docs/decisions/connection-quality-ladder.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CarrierPolicy {
    /// «TCP» — every hop of ours over TLS-over-TCP: the relay through its TCP bridge, a contact
    /// through its TCP door. No UDP dial, no climb; we advertise our door, not our UDP addresses.
    /// A contact's raw-UDP dial is refused too. Survives a network that cuts UDP; slower
    /// (head-of-line blocking).
    Tcp,
    /// «Авто» (default) — come up on what is reliable, climb to QUIC where it works, fall back when
    /// it degrades, on every hop.
    #[default]
    Auto,
    /// «QUIC» — every hop of ours over QUIC/UDP; no TCP relay carrier, no door, and a dialer coming
    /// through one is refused. Fastest; dead where UDP is blocked.
    Quic,
}

impl CarrierPolicy {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Auto => "auto",
            Self::Quic => "quic",
        }
    }

    /// Reads the old names too (`relay_tcp_only` — TCP to the relay only, `quic_forced`), which the
    /// two modes replaced.
    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "tcp" | "relay_tcp_only" => Ok(Self::Tcp),
            "auto" => Ok(Self::Auto),
            "quic" | "quic_forced" => Ok(Self::Quic),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported carrier policy '{}'",
                other
            ))),
        }
    }
}

// NB: the connection-quality ladder's privacy axis ("never reveal my IP — relay only" vs
// "direct allowed") is NOT a separate type here — it is unified into the existing
// `omni_app::ConnectionPrivacyMode` (Auto / DirectOnly / RelayOnly), the one privacy knob, to
// avoid two contradictory settings for the same direct-vs-relay choice. `CarrierPolicy` above is
// the orthogonal speed/reliability axis. See docs/decisions/connection-quality-ladder.md §"Settings".

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportSnapshot {
    pub state: TransportState,
    pub transport_mode: TransportMode,
    /// The mesh node of the active connection (hex).
    pub peer_id: Option<String>,
    #[serde(rename = "peer_key", alias = "dialog_key")]
    pub dialog_key: Option<String>,
    pub remote_instance_id: Option<String>,
    pub remote_device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_account_id: Option<String>,
    #[serde(default)]
    pub initiated_locally: bool,
    #[serde(default)]
    pub duplicate_resolved: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportCommandResult {
    pub ok: bool,
    pub message: String,
    pub snapshot: TransportSnapshot,
}

pub fn normalize_dialog_key(dialog_key: &str) -> String {
    dialog_key.trim().to_string()
}

pub fn dialog_key_from_instance_id(instance_id: &str) -> String {
    format!("{DIALOG_KEY_INSTANCE_PREFIX}{}", instance_id.trim())
}

pub fn dialog_key_from_account_id(account_id: &str) -> String {
    format!("{DIALOG_KEY_ACCOUNT_PREFIX}{}", account_id.trim())
}

pub fn instance_id_from_dialog_key(dialog_key: &str) -> Option<String> {
    normalize_dialog_key(dialog_key)
        .strip_prefix(DIALOG_KEY_INSTANCE_PREFIX)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn account_id_from_dialog_key(dialog_key: &str) -> Option<String> {
    normalize_dialog_key(dialog_key)
        .strip_prefix(DIALOG_KEY_ACCOUNT_PREFIX)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn is_dialog_key(selector: &str) -> bool {
    let trimmed = selector.trim();
    trimmed.starts_with(DIALOG_KEY_INSTANCE_PREFIX)
        || trimmed.starts_with(DIALOG_KEY_ACCOUNT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_policy_db_round_trips_all_variants() {
        for p in [
            CarrierPolicy::Tcp,
            CarrierPolicy::Auto,
            CarrierPolicy::Quic,
        ] {
            assert_eq!(CarrierPolicy::from_db_value(p.as_db_value()).unwrap(), p);
        }
    }

    #[test]
    fn db_value_is_trimmed_and_rejects_unknown() {
        assert_eq!(
            CarrierPolicy::from_db_value("  auto ").unwrap(),
            CarrierPolicy::Auto
        );
        assert!(CarrierPolicy::from_db_value("bogus").is_err());
        assert!(CarrierPolicy::from_db_value("").is_err());
    }

    #[test]
    fn default_carrier_is_reliable_base_then_opportunistic_climb() {
        // Default = come up on the rock-solid TCP base, then climb to the fastest healthy rung.
        assert_eq!(CarrierPolicy::default(), CarrierPolicy::Auto);
    }
}
