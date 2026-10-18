// Discoverability (global identity discovery / Model A, MA1). A SECOND privacy
// axis, orthogonal to ConnectionPrivacyMode (which is *how* I connect): this is
// *who may find/initiate by my ID*. Default `Off` keeps Model B (invisible
// without a shared pair_secret) as the default. `Public` opts into Model A —
// anyone holding my Omni-ID can find me (still request→accept, not auto-admit).
// See docs/decisions/global-identity-discovery.md §7.
//
// Persisted per-profile in a sidecar file (no schema migration), mirroring
// ConnectionPrivacyMode.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverabilityMode {
    /// Default: not findable by ID — only peers who already share a pair_secret
    /// (Model B) can rendezvous.
    Off,
    /// Findable by anyone holding my Omni-ID (Model A). Sessions are still
    /// request→accept (consent layer), not auto-admitted.
    Public,
}

impl DiscoverabilityMode {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Public => "public",
        }
    }

    pub fn from_db_value(value: &str) -> DiscoverabilityMode {
        match value.trim() {
            "public" => Self::Public,
            _ => Self::Off,
        }
    }

    pub fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }
}

fn discoverability_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "discoverability-mode.json")
}

/// Present while the withdrawal of our identity beacon is still to be posted ("findable by ID" was
/// turned off; the next sweep posts it and removes this). On disk, so a restart in between does
/// not leave the last real beacon findable.
fn identity_withdrawal_marker_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "identity-beacon-withdrawal.json")
}

pub(crate) fn identity_withdrawal_pending(db_path: &str) -> bool {
    identity_withdrawal_marker_path(db_path).exists()
}

pub(crate) fn clear_identity_withdrawal(db_path: &str) {
    let _ = std::fs::remove_file(identity_withdrawal_marker_path(db_path));
}

pub(crate) fn load_discoverability_mode(db_path: &str) -> DiscoverabilityMode {
    match std::fs::read_to_string(discoverability_sidecar_path(db_path)) {
        Ok(raw) => DiscoverabilityMode::from_db_value(raw.trim().trim_matches('"')),
        Err(_) => DiscoverabilityMode::Off,
    }
}

impl OmniApp {
    /// My shareable Omni-ID (`omni:<40 hex>`) — a commitment to my account's master key, the same
    /// on every device of the account and after a restore from the phrase. Give it to someone
    /// (paste/QR) and, if my discoverability is `Public`, they can find me and send a contact
    /// request. See global-identity-discovery.md, multi-device-equal-nodes.md.
    pub fn get_my_omni_id(&self) -> OmniResult<String> {
        let account = self
            .storage
            .get_local_account()?
            .ok_or_else(|| OmniError::InvalidInput("no local account".to_string()))?;
        let keys = self
            .storage
            .get_account_keys(&account.descriptor.account_id)?
            .ok_or_else(|| {
                OmniError::InvalidInput(
                    "this profile was created before the account key chain: create it again"
                        .to_string(),
                )
            })?;
        Ok(omni_crypto::omni_id_string(&keys.master_public_key))
    }

    /// Find a peer by their Omni-ID (Model A, MA3) and send a contact request — the
    /// "add anyone by ID, no prior exchange" path. Fetches their signed identity
    /// beacon from the couriers, verifies it against the ID (commitment + signature),
    /// records their prekey bundle, creates an outgoing pending contact, and kicks off
    /// the mesh connection (the outgoing request ships on link-up). The peer still has
    /// to ACCEPT — this is request→accept, not auto-admit. They must have opted into
    /// `Public` discoverability, or no beacon exists to find.
    /// See docs/decisions/global-identity-discovery.md.
    pub fn find_contact_by_omni_id(
        &self,
        omni_id_str: &str,
        petname: impl Into<String>,
    ) -> OmniResult<omni_identity::ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("find_contact_by_omni_id")?;
        let omni_id = omni_crypto::parse_omni_id(omni_id_str).ok_or_else(|| {
            OmniError::InvalidInput("not a valid Omni-ID (expected omni:<40 hex>)".to_string())
        })?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();

        let rendezvous = profile_rendezvous_runtime(&db_path)?;
        // Three outcomes, three different things to tell the user. Collapsing the last two
        // into one message is how a courier outage gets reported as "this person is not
        // findable" — a statement about someone else's settings that we have no evidence
        // for. Live 2026-08-11: ntfy failed on every call while only nostr answered.
        let beacons = match lookup_identity_beacon(rendezvous.helper.as_ref(), &omni_id)? {
            IdentityBeaconLookup::Found(beacons) => beacons,
            IdentityBeaconLookup::NotPublished => {
                return Err(OmniError::InvalidInput(
                    "no identity beacon found for this ID — the peer is either not findable-by-ID \
                     (discoverability is off by default, they must turn it on) or offline"
                        .to_string(),
                ))
            }
            IdentityBeaconLookup::Unreachable => {
                return Err(OmniError::Transport(
                    "could not reach any rendezvous courier, so nothing could be looked up — \
                     check connectivity and retry"
                        .to_string(),
                ))
            }
        };

        // Every device of the account that is findable has its own beacon; each is verified
        // (commitment + signature) inside the lookup. Each carries that device's prekey bundle —
        // the X3DH material for its own session; validate + persist them all. A malformed one
        // costs only that device.
        let mut devices: Vec<(omni_rendezvous::IdentityBeacon, String)> = Vec::new();
        let mut last_error = None;
        for beacon in beacons {
            match self.record_identity_beacon_bundle(&beacon) {
                Ok(account_id) => devices.push((beacon, account_id)),
                Err(error) => {
                    omni_app_trace_log("find_by_id/beacon", &format!("device skipped: {error}"));
                    last_error = Some(error);
                }
            }
        }
        let Some(account_id) = devices.first().map(|(_, account_id)| account_id.clone()) else {
            return Err(last_error.unwrap_or_else(|| {
                OmniError::InvalidInput("no usable identity beacon for this ID".to_string())
            }));
        };

        // Create the outgoing pending contact (consent-gated on the far side). This gives
        // the caller a synchronous snapshot with the chosen petname; the far side learns of
        // the request only when the bootstrap ships below. No name given: the account id, the
        // placeholder the shell shows the person's own name over once their profile arrives (a
        // beacon carries no name; «Новый контакт» stuck for good, stand 2026-10-13).
        let petname = petname.into();
        let petname = if petname.trim().is_empty() { account_id.clone() } else { petname };
        let snapshot = self.request_contact_by_identifier(account_id, petname, None)?;
        if let Some(runtime) = self.mesh_runtime() {
            let hello_bytes =
                serde_json::to_vec(&self.transport.build_local_session_hello_envelope())
                    .unwrap_or_default();
            let allow_direct = crate::app_pinned_relay::allows_direct(&self.storage.db_path().to_string_lossy());
            // Every device: the request reaches whichever of them is up, and each gets its session.
            for (beacon, _) in &devices {
                dial_identity_beacon_target(
                    &runtime,
                    &self.storage,
                    &self.mesh_pairing_targets,
                    beacon,
                    MeshNodeAddress::from_bytes(beacon.node),
                    &hello_bytes,
                    allow_direct,
                );
            }
        }
        Ok(snapshot)
    }

    /// Validate and persist the prekey bundle an identity beacon carries; the account it is for.
    fn record_identity_beacon_bundle(
        &self,
        beacon: &omni_rendezvous::IdentityBeacon,
    ) -> OmniResult<String> {
        let bundle_v1 = identity_beacon_prekey_bundle(beacon)?;
        omni_protocol::validate_prekey_bundle_v1(&bundle_v1)?;
        let domain_bundle = prekey_bundle_from_protocol(&bundle_v1)?;
        // The bundle must be the one of the device that signed the beacon.
        let beacon_device = identity_beacon_device_chain(beacon)?.device;
        if domain_bundle.identity_signing_key != beacon_device.identity_signing_public_key
            || domain_bundle.device_certificate_chain.is_none()
        {
            return Err(OmniError::InvalidInput(
                "identity beacon carries another device's prekey bundle".to_string(),
            ));
        }
        self.storage.record_remote_prekey_bundle(
            &domain_bundle,
            Some(&bundle_v1.account.account_id),
            None,
            current_unix_ms(),
        )?;
        Ok(bundle_v1.account.account_id)
    }

    pub fn discoverability_mode(&self) -> DiscoverabilityMode {
        load_discoverability_mode(&self.storage.db_path().to_string_lossy())
    }

    /// Set whether I'm findable by my Omni-ID (Model A opt-in). Persisted in the
    /// per-profile sidecar; the rendezvous sweep reads it live, so a change takes
    /// effect on the next sweep without a mesh restart.
    ///
    /// Off posts a withdrawal of the last beacon (next sweep, marker on disk until it is out), so
    /// I stop being findable right away rather than when the old beacon expires. On posts the
    /// beacon on the next sweep.
    pub fn set_discoverability_mode(&self, mode: DiscoverabilityMode) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_discoverability_mode")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let was_public = load_discoverability_mode(&db_path).is_public();
        let path = discoverability_sidecar_path(&db_path);
        std::fs::write(&path, format!("\"{}\"", mode.as_db_value())).map_err(|error| {
            OmniError::Storage(format!("failed to persist discoverability mode: {error}"))
        })?;
        if mode.is_public() {
            clear_identity_withdrawal(&db_path);
            forget_identity_beacon_publish(&db_path);
        } else if was_public {
            std::fs::write(identity_withdrawal_marker_path(&db_path), "{}").map_err(|error| {
                OmniError::Storage(format!("failed to persist the beacon withdrawal: {error}"))
            })?;
        }
        Ok(())
    }
}

/// Register `node` as a mesh pairing target and dial its identity-beacon endpoints — the shared
/// body used BOTH by the synchronous `find_contact_by_omni_id` (first attempt) and the background
/// retry sweep (`retry_unlinked_outgoing_id_pendings`). Extracted so the two can't drift.
///
/// Why each step (cross-network relay-less reach, 2026-07-20):
///  - pairing target FIRST: the on-link `SessionHello` handler runs `finish_mesh_pairing_from_hello`
///    → builds the initiator X3DH session + ships the bootstrap that CARRIES the contact request.
///  - relay bundle unconditionally (even RelayOnly): relay propagation exists exactly for when we
///    cannot reach directly, so it must not depend on a reachable endpoint.
///  - direct dials gated by `allow_direct`: `dial_at` (UDP-direct/reflexive + UPnP) and
///    `dial_peer_tcp_direct` (TLS-over-TCP front door — the path that survives a mobile carrier
///    dropping UDP). The beacon now carries the optimistic UPnP-TCP front door (`beacon_tcp_direct`).
///  - `runtime.dial(node)` fallback: announce-routed / multi-hop / relay path.
///  - hello push when already linked: a coalesced re-dial emits no fresh hello, so pairing stalls
///    without this (the second-add-after-forget failure).
/// Every outcome is traced (`find_by_id/*`) — the errors used to be swallowed, which is exactly
/// why the live "never delivered" was undiagnosable.
fn dial_identity_beacon_target(
    runtime: &MeshRuntime,
    storage: &Storage,
    pairing_targets: &std::sync::Mutex<std::collections::HashSet<MeshNodeAddress>>,
    beacon: &omni_rendezvous::IdentityBeacon,
    node: MeshNodeAddress,
    hello_bytes: &[u8],
    allow_direct: bool,
) {
    if let Ok(mut targets) = pairing_targets.lock() {
        targets.insert(node);
    }

    if let Some(token) = beacon.relay_bundle.as_deref() {
        let issuer = node.to_string();
        match apply_relay_bundle(storage, token, Some(&issuer)) {
            Ok(n) => omni_app_trace_log(
                "find_by_id/relay-bundle",
                &format!("applied {n} relay(s) from {issuer}"),
            ),
            Err(error) => omni_app_trace_log(
                "find_by_id/relay-bundle",
                &format!("rejected relay bundle from {issuer}: {error}"),
            ),
        }
    }

    omni_app_trace_log(
        "find_by_id/beacon",
        &format!(
            "node={node} reflexive={:?} tcp_direct={:?} relay_bundle={}",
            beacon.reflexive,
            beacon.tcp_direct,
            beacon.relay_bundle.is_some()
        ),
    );

    if runtime.has_link(&node) {
        let sent = !hello_bytes.is_empty() && runtime.send_wire(node, hello_bytes).is_ok();
        omni_app_trace_log(
            "find_by_id/dial",
            &format!("node={node} already_linked hello_pushed={sent}"),
        );
    } else if allow_direct {
        let udp_endpoints: Vec<std::net::SocketAddr> =
            beacon.reflexive.iter().filter_map(|s| s.parse().ok()).collect();
        let tcp_endpoints: Vec<std::net::SocketAddr> =
            beacon.tcp_direct.iter().filter_map(|s| s.parse().ok()).collect();
        // S3 honest failover: remember the peer's direct-TCP door so a later phantom-drop can
        // migrate this peer's connection onto it (the reliable base on a UDP-hostile carrier),
        // instead of the inert UDP demote. Learned here at pairing; refreshed each approved sweep.
        if let Some(door) = tcp_endpoints.first() {
            crate::app_peer_door::set_peer_door(
                &storage.db_path().to_string_lossy(),
                node.as_bytes(),
                *door,
            );
        }
        if !udp_endpoints.is_empty() {
            match runtime.dial_at(node, udp_endpoints.clone()) {
                Ok(()) => omni_app_trace_log(
                    "find_by_id/dial",
                    &format!("node={node} udp_direct OK endpoints={udp_endpoints:?}"),
                ),
                Err(error) => omni_app_trace_log(
                    "find_by_id/dial",
                    &format!("node={node} udp_direct FAILED endpoints={udp_endpoints:?}: {error}"),
                ),
            }
        }
        if !tcp_endpoints.is_empty() {
            match runtime.dial_peer_tcp_direct(node, tcp_endpoints.clone()) {
                Ok(true) => omni_app_trace_log(
                    "find_by_id/dial",
                    &format!("node={node} tcp_direct OK endpoints={tcp_endpoints:?}"),
                ),
                // Nothing was migrated: the door is a route for an EXISTING connection, and there
                // is none yet. Reported honestly instead of as `OK`, which is what made a search
                // that found the peer look like it had also reached it.
                Ok(false) => omni_app_trace_log(
                    "find_by_id/dial",
                    &format!(
                        "node={node} tcp_direct NOT-ROUTED (no live connection to migrate) endpoints={tcp_endpoints:?}"
                    ),
                ),
                Err(error) => omni_app_trace_log(
                    "find_by_id/dial",
                    &format!("node={node} tcp_direct FAILED endpoints={tcp_endpoints:?}: {error}"),
                ),
            }
        }
        if udp_endpoints.is_empty() && tcp_endpoints.is_empty() {
            omni_app_trace_log(
                "find_by_id/dial",
                &format!("node={node} NO direct endpoints in beacon — relying on relay/announce fallback"),
            );
        }
    } else {
        omni_app_trace_log(
            "find_by_id/dial",
            &format!("node={node} direct dial skipped (RelayOnly privacy) — relay/announce only"),
        );
    }

    // Announce-routed / multi-hop / relay fallback. No-op if a direct dial above already linked us.
    let _ = runtime.dial(node);
    omni_app_trace_log(
        "find_by_id/result",
        &format!("node={node} linked_now={}", runtime.has_link(&node)),
    );
}

#[cfg(test)]
mod discoverability_tests {
    use super::*;

    #[test]
    fn discoverability_mode_db_value_roundtrip_and_default_off() {
        assert_eq!(DiscoverabilityMode::from_db_value("public"), DiscoverabilityMode::Public);
        assert_eq!(DiscoverabilityMode::from_db_value("off"), DiscoverabilityMode::Off);
        assert_eq!(DiscoverabilityMode::from_db_value("garbage"), DiscoverabilityMode::Off);
        assert!(DiscoverabilityMode::Public.is_public());
        assert!(!DiscoverabilityMode::Off.is_public());
        assert_eq!(DiscoverabilityMode::Public.as_db_value(), "public");
    }
}
