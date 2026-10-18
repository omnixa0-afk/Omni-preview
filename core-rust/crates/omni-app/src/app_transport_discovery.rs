#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportDiscoveryRefreshSnapshot {
    pub transport_snapshot: TransportSnapshot,
    pub queue_diagnostics: QueueDiagnosticsSnapshot,
}

impl OmniApp {
    pub fn disconnect_transport(&self) -> OmniResult<TransportCommandResult> {
        self.transport.disconnect()
    }

    pub fn transport_snapshot(&self) -> TransportSnapshot {
        self.transport.snapshot()
    }

    pub fn transport_discovery_refresh_snapshot(
        &self,
    ) -> OmniResult<TransportDiscoveryRefreshSnapshot> {
        let transport_snapshot = self.transport.snapshot();
        let queue_diagnostics = self
            .storage
            .queue_diagnostics(transport_snapshot.last_error.clone())?;
        Ok(TransportDiscoveryRefreshSnapshot {
            transport_snapshot,
            queue_diagnostics,
        })
    }

    pub fn outbound_queue_snapshot(&self) -> OmniResult<OutboundQueueSnapshot> {
        self.storage.outbound_queue_snapshot()
    }

    pub fn queue_diagnostics(&self) -> OmniResult<QueueDiagnosticsSnapshot> {
        self.storage
            .queue_diagnostics(self.transport.snapshot().last_error.clone())
    }

    /// The `SessionHello` this node sends on every new mesh link carries its prekey bundle; keep
    /// the transport's copy in step with the signed-in profile.
    fn sync_transport_local_identity(&self) -> OmniResult<()> {
        if !self.storage.is_profile_signed_in()? {
            self.clear_transport_local_identity();
            return Ok(());
        }
        self.refresh_transport_prekey_bundle()
    }

    fn clear_transport_local_identity(&self) {
        self.transport.set_local_prekey_bundle_json(None);
    }

    fn refresh_transport_prekey_bundle(&self) -> OmniResult<()> {
        let bundle = self.get_local_prekey_bundle()?;
        let bundle_json = serde_json::to_string(&bundle).map_err(|error| {
            OmniError::Internal(format!(
                "failed to serialize local prekey bundle for transport handshake: {error}"
            ))
        })?;
        self.transport.set_local_prekey_bundle_json(Some(bundle_json));
        Ok(())
    }
}
