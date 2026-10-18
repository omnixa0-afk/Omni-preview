use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetDiscoverabilityRequest {
    /// "off" | "public"
    pub(crate) mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FindByOmniIdRequest {
    pub(crate) omni_id: String,
    pub(crate) petname: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetNameFindableRequest {
    pub(crate) on: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchByNameRequest {
    pub(crate) query: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetPublicRelayRoleRequest {
    pub(crate) on: bool,
    #[serde(default)]
    pub(crate) name: String,
}
