#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportRequestPayloadV1 {
    SessionHello(SessionHelloV1),
    WireEnvelope {
        envelope: WireEnvelopeV1,
    },
    /// A group durable-feed entry: `item_json` is a `GroupFeedItemV2` carrying one MLS message
    /// (feed = "mls"). Needs no session ctx — MLS replaces the pairwise ratchet
    /// (docs/decisions/group-crypto-mls.md).
    GroupFeed { feed: String, item_json: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportRequestEnvelopeV1 {
    pub protocol_version: u16,
    #[serde(flatten)]
    pub payload: TransportRequestPayloadV1,
}

impl TransportRequestEnvelopeV1 {
    pub fn session_hello(
        instance_id: impl Into<String>,
        transport_mode: TransportMode,
        device_name: impl Into<String>,
        account_id: Option<String>,
        prekey_bundle_json: Option<String>,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: TransportRequestPayloadV1::SessionHello(SessionHelloV1 {
                instance_id: instance_id.into(),
                transport_mode,
                device_name: device_name.into(),
                account_id,
                prekey_bundle_json,
            }),
        }
    }

    pub fn wire_envelope(envelope: WireEnvelopeV1) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: TransportRequestPayloadV1::WireEnvelope { envelope },
        }
    }

    pub fn group_feed(feed: impl Into<String>, item_json: impl Into<String>) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: TransportRequestPayloadV1::GroupFeed {
                feed: feed.into(),
                item_json: item_json.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportResponsePayloadV1 {
    SessionHello(SessionHelloV1),
    Ack(TransportAckV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportResponseEnvelopeV1 {
    pub protocol_version: u16,
    #[serde(flatten)]
    pub payload: TransportResponsePayloadV1,
}

impl TransportResponseEnvelopeV1 {
    pub fn session_hello(
        instance_id: impl Into<String>,
        transport_mode: TransportMode,
        device_name: impl Into<String>,
        account_id: Option<String>,
        prekey_bundle_json: Option<String>,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: TransportResponsePayloadV1::SessionHello(SessionHelloV1 {
                instance_id: instance_id.into(),
                transport_mode,
                device_name: device_name.into(),
                account_id,
                prekey_bundle_json,
            }),
        }
    }

    pub fn ack(received_at_unix_ms: i64) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: TransportResponsePayloadV1::Ack(TransportAckV1 {
                received_at_unix_ms,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportAckV1 {
    pub received_at_unix_ms: i64,
}

pub fn validate_remote_prekey_bundle(
    bundle_json: Option<&str>,
) -> OmniResult<Option<PrekeyBundleV1>> {
    let Some(bundle_json) = bundle_json else {
        return Ok(None);
    };

    let bundle: PrekeyBundleV1 = serde_json::from_str(bundle_json).map_err(|error| {
        OmniError::Transport(format!("failed to parse remote prekey bundle: {error}"))
    })?;
    omni_protocol::validate_prekey_bundle_v1(&bundle)?;
    Ok(Some(bundle))
}
