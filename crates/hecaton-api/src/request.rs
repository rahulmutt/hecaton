//! Request bodies for the fleet endpoints (spec §7).

use serde::{Deserialize, Serialize};

use crate::{CredentialBundle, FleetSpec};

/// Body of `POST /v1/fleets` (up) and `PUT /v1/fleets/{name}` (update).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRequest {
    pub spec: FleetSpec,
    #[serde(default)]
    pub credentials: CredentialBundle,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn request_round_trips() {
        let r = FleetRequest {
            spec: FleetSpec {
                name: "f".into(),
                crews: BTreeMap::new(),
            },
            credentials: CredentialBundle::default(),
        };
        let back: FleetRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.spec, r.spec);
        assert_eq!(back.credentials.gh_token, None);
    }
}
