//! The client's half of `scope.mirror`: handing every remote server the scope
//! this client filters by.
//!
//! Plugins run on the server that hosts their pane and see only that server.
//! Find on a studio pane asks studio's server `scope.get`, and that server, on
//! its own, would judge by its own scope, which holds only its config hides. So
//! the client mirrors its scope to each remote server, naming the machine as its
//! own panels do, and the server judges with the same predicate on the same
//! names (`HeadlessServer::scope_get_judged_by_mirror`).
//!
//! ```text
//!  Local scope.projection ─► set_scope_projection ─► scope_changed ─┐
//!  remote snapshot or status (online, new connection, new boot) ────┤
//!  catalog reload (a machine's label) ──────────────────────────────┤
//!                                                                   ▼
//!   sync_scope_mirrors: one scope.mirror per online remote endpoint that
//!   advertises it, unless that exact mirror already went to that connection
//!                                                                   │ scope_requests
//!                                                                   ▼
//!   client loop dispatch: onto that endpoint's lane even while it is not
//!   presented; a newer mirror supersedes a queued older one; lane retirement
//!   keeps it; its result is settled here, silently
//! ```
//!
//! The mirror carries `effective_scope()`, so the config-only fallback (a Local
//! without `scope.projection`) mirrors too. Failures only log: the panels are
//! already right, and only plugin lists on that machine lag. A server that
//! refuses the method is never asked again for that boot.
use crate::api::schema::{Method, Request};
use crate::scope::ScopeMirrorParams;

use super::*;

/// The method name a remote server advertises when it takes a mirror.
const SCOPE_MIRROR_METHOD: &str = "scope.mirror";

/// Error codes that mean the server does not have `scope.mirror` (an older
/// build, or one that cannot read it): asking again for that boot is pointless.
const MIRROR_REFUSED_CODES: &[&str] = &[
    "unsupported_method",
    "unsupported_endpoint_command",
    "unknown_method",
    "invalid_request",
];

/// The last mirror queued for one remote endpoint: what went to which
/// connection (`generation`) and server boot. A reconnect, even to the same
/// boot, is a new server-side connection with no mirror, so the generation is
/// part of the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeMirrorSent {
    generation: Option<u64>,
    boot_id: String,
    params: ScopeMirrorParams,
}

/// One mirror the sync decided to queue.
struct PendingMirror {
    endpoint_id: ClientEndpointId,
    sent: ScopeMirrorSent,
}

impl ClientShellState {
    /// Queues a `scope.mirror` for every online remote endpoint whose current
    /// connection has not had this exact one. Cheap when nothing changed: one
    /// comparison per remote endpoint, no clone. Runs on every scope change and
    /// whenever a remote endpoint's snapshot, status, methods or label change.
    pub(super) fn sync_scope_mirrors(&mut self) {
        let mut pending = Vec::new();
        {
            let scope = self.effective_scope();
            for endpoint in &self.endpoints {
                if endpoint.endpoint_id.is_local()
                    || endpoint.status != ClientEndpointStatus::Online
                {
                    continue;
                }
                let Some(snapshot) = endpoint.snapshot.as_deref() else {
                    continue;
                };
                // Only a server that advertises the method gets it: an older one
                // would reject it as unknown.
                if !endpoint
                    .methods
                    .as_ref()
                    .is_some_and(|methods| methods.contains(SCOPE_MIRROR_METHOD))
                {
                    continue;
                }
                if self.scope_mirror_refused.get(&endpoint.endpoint_id) == Some(&snapshot.boot_id) {
                    continue;
                }
                let machine = endpoint.endpoint_id.storage_key();
                let unchanged = self
                    .scope_mirrors_sent
                    .get(&endpoint.endpoint_id)
                    .is_some_and(|sent| {
                        sent.generation == endpoint.snapshot_generation
                            && sent.boot_id == snapshot.boot_id
                            && sent.params.scope.as_ref() == Some(&*scope)
                            && sent.params.machine == machine
                            && sent.params.machine_label == endpoint.label
                    });
                if unchanged {
                    continue;
                }
                pending.push(PendingMirror {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    sent: ScopeMirrorSent {
                        generation: endpoint.snapshot_generation,
                        boot_id: snapshot.boot_id.clone(),
                        params: ScopeMirrorParams {
                            scope: Some(scope.as_ref().clone()),
                            machine,
                            machine_label: endpoint.label.clone(),
                        },
                    },
                });
            }
        }
        for mirror in pending {
            self.queue_scope_mirror(mirror);
        }
    }

    /// Queues one mirror beside Local's scope writes (`scope_requests`). A
    /// mirror for the same endpoint still waiting there is dropped first, so the
    /// stale scope never follows the fresh one; the lane does the same for one
    /// already handed to it (`EndpointCommands::enqueue`).
    fn queue_scope_mirror(&mut self, mirror: PendingMirror) {
        let PendingMirror { endpoint_id, sent } = mirror;
        let mut superseded = Vec::new();
        self.scope_requests.retain(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id: queued_endpoint,
                request,
                ..
            } if queued_endpoint == &endpoint_id && is_scope_mirror(request) => {
                superseded.push(request.id.clone());
                false
            }
            _ => true,
        });
        for request_id in superseded {
            self.scope_mirror_request_ids.remove(&request_id);
        }
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.scope_mirror_request_ids.insert(
            request_id.clone(),
            (endpoint_id.clone(), sent.boot_id.clone()),
        );
        self.scope_requests.push(ClientShellAction::Endpoint {
            endpoint_id: endpoint_id.clone(),
            boot_id: sent.boot_id.clone(),
            request: Box::new(Request {
                id: request_id,
                method: Method::ScopeMirror(sent.params.clone()),
            }),
        });
        self.scope_mirrors_sent.insert(endpoint_id, sent);
    }

    /// Whether the shell holds scope requests the client loop has not
    /// dispatched yet (queued outside an input path, e.g. by a snapshot).
    pub(crate) fn has_scope_requests(&self) -> bool {
        !self.scope_requests.is_empty()
    }

    /// Settles a mirror's result, if `request_id` is one: success, rejection,
    /// timeout and cancellation (superseded, lane gone) all stay silent apart
    /// from a debug log. A refusal of the method is remembered for that boot.
    /// Returns `None` for ids that are not mirrors.
    pub(super) fn finish_scope_mirror(
        &mut self,
        request_id: &str,
        result: &Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> Option<bool> {
        let (endpoint_id, boot_id) = self.scope_mirror_request_ids.remove(request_id)?;
        if let Err(error) = result {
            let code = error.code.as_deref().unwrap_or("invalid_response");
            tracing::debug!(
                endpoint = %endpoint_id.storage_key(),
                code,
                message = %error.message,
                "scope mirror not applied; plugin lists on that machine may lag"
            );
            if MIRROR_REFUSED_CODES.contains(&code) {
                self.scope_mirror_refused.insert(endpoint_id, boot_id);
            }
        }
        Some(false)
    }

    /// Whether `request_id` is an unsettled mirror.
    pub(super) fn owns_scope_mirror_request(&self, request_id: &str) -> bool {
        self.scope_mirror_request_ids.contains_key(request_id)
    }

    /// The mirror last queued for `endpoint_id`, for tests.
    #[cfg(test)]
    pub(crate) fn scope_mirror_sent_to(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&ScopeMirrorParams> {
        self.scope_mirrors_sent
            .get(endpoint_id)
            .map(|sent| &sent.params)
    }
}

fn is_scope_mirror(request: &Request) -> bool {
    matches!(request.method, Method::ScopeMirror(_))
}
