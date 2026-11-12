//! Optional server draft gates, isolated by endpoint, boot and connection. The
//! client only paints these facts; draft timing and prompt delivery stay server-owned.

use std::collections::BTreeMap;

use super::*;
use crate::protocol::endpoint::EndpointPromptHolds;

/// One pane's hold state as the server projected it. Presence in the map
/// means the pane is holding agent prompts behind a human draft.
pub(super) struct PromptHold {
    pub held: usize,
}

/// A full replacement belongs to exactly one incarnation of one connection.
struct Projection {
    generation: u64,
    boot: String,
    holds: BTreeMap<String, PromptHold>,
}

/// Retains inactive endpoints' controls until their matching snapshot is shown.
#[derive(Default)]
pub(super) struct PromptHolds {
    endpoints: HashMap<ClientEndpointId, Projection>,
}

impl ClientShellState {
    pub(crate) fn set_endpoint_prompt_holds(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        projection: EndpointPromptHolds,
    ) {
        if self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint)
            .and_then(|entry| entry.snapshot_generation)
            .is_some_and(|current| current > generation)
        {
            return;
        }
        self.prompt_holds.endpoints.insert(
            endpoint.clone(),
            Projection {
                generation,
                boot: projection.boot_id,
                holds: projection
                    .holds
                    .into_iter()
                    .map(|(pane, hold)| (pane, PromptHold { held: hold.held }))
                    .collect(),
            },
        );
    }

    /// Holds for the presented endpoint, keyed by public pane id.
    pub(super) fn prompt_holds(&self) -> Option<&BTreeMap<String, PromptHold>> {
        let snapshot = self.snapshot.as_deref()?;
        self.prompt_holds
            .endpoints
            .get(&self.active_endpoint_id)
            .filter(|projection| {
                projection.boot == snapshot.boot_id
                    && self
                        .active_snapshot_generation
                        .is_none_or(|generation| generation == projection.generation)
            })
            .map(|projection| &projection.holds)
    }
}
