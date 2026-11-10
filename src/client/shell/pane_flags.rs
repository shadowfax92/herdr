//! Server-owned pane flags and the client's definition of a visit.
//!
//! Projections are keyed by endpoint, boot and connection generation; rendering
//! only reads the presented endpoint. Focus arrival arms a one-second deadline,
//! paused while the host lacks focus. Projection updates never manufacture an
//! arrival, so flagging the pane the owner already has focused stays visible.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::{Method, PaneFlag, PaneUnflagParams};
use crate::protocol::endpoint::EndpointPaneFlags;

pub(super) const PANE_FLAG_DWELL: Duration = Duration::from_millis(1000);

/// Focus identity, including its server incarnation, across projection updates.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VisitTarget {
    endpoint: ClientEndpointId,
    boot: String,
    generation: Option<u64>,
    pane: String,
}

/// One connection's latest full replacement, independently of which is shown.
#[derive(Default)]
struct Projection {
    generation: u64,
    boot: String,
    flags: BTreeMap<String, PaneFlag>,
}

/// Remaining focused time for one specific flag generation.
struct Dwell {
    target: VisitTarget,
    seq: u64,
    remaining: Duration,
    deadline: Option<Instant>,
}

/// Client presentation and visit bookkeeping. Flags remain authoritative on
/// the server; a clear sends an advertised API request, never a local mutation.
#[derive(Default)]
pub(super) struct PaneFlags {
    endpoints: HashMap<ClientEndpointId, Projection>,
    any_flags: bool,
    input_suspended: bool,
    observed: Option<VisitTarget>,
    arrival: bool,
    dwell: Option<Dwell>,
    requested: HashMap<String, (VisitTarget, u64)>,
    #[cfg(test)]
    pub(super) clock: Option<Instant>,
}

impl PaneFlags {
    pub(super) fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.clock {
            return now;
        }
        Instant::now()
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        if self.input_suspended {
            return None;
        }
        self.dwell.as_ref().and_then(|dwell| dwell.deadline)
    }
}

impl ClientShellState {
    pub(crate) fn set_endpoint_pane_flags(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        projection: EndpointPaneFlags,
    ) {
        // The loop rejects retired connections before decoding controls. Keep
        // the generation here too, because panes may reuse ids after reconnect.
        if self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint)
            .and_then(|entry| entry.snapshot_generation)
            .is_some_and(|current| current > generation)
        {
            return;
        }
        self.pane_flags.endpoints.insert(
            endpoint.clone(),
            Projection {
                generation,
                boot: projection.boot_id,
                flags: projection
                    .flags
                    .into_iter()
                    .map(|entry| (entry.pane_id, entry.flag))
                    .collect(),
            },
        );
        self.pane_flags.any_flags = self
            .pane_flags
            .endpoints
            .values()
            .any(|projection| !projection.flags.is_empty());
        self.reconcile_pane_flag_visit(None);
    }

    fn presented_flag_projection(&self) -> Option<&Projection> {
        let snapshot = self.snapshot.as_deref()?;
        self.pane_flags
            .endpoints
            .get(&self.active_endpoint_id)
            .filter(|projection| {
                projection.boot == snapshot.boot_id
                    && self
                        .active_snapshot_generation
                        .is_none_or(|generation| generation == projection.generation)
            })
    }

    /// The UX seam: only flags belonging to the presented endpoint and boot.
    pub(crate) fn pane_flags(&self) -> Option<&BTreeMap<String, PaneFlag>> {
        self.presented_flag_projection()
            .map(|projection| &projection.flags)
    }

    /// Tab markers use pane membership, not the agent list: shell panes count.
    #[allow(dead_code)] // Public renderer seam consumed by the stacked UX change.
    pub(crate) fn tab_has_pane_flags(&self, tab_id: &str) -> bool {
        let Some(flags) = self.pane_flags().filter(|flags| !flags.is_empty()) else {
            return false;
        };
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .panes
                .iter()
                .any(|pane| pane.tab_id == tab_id && flags.contains_key(&pane.pane_id))
        })
    }

    fn pane_flag_target(&self) -> Option<VisitTarget> {
        let snapshot = self.snapshot.as_ref()?;
        Some(VisitTarget {
            endpoint: self.active_endpoint_id.clone(),
            boot: snapshot.boot_id.clone(),
            generation: self.active_snapshot_generation,
            pane: snapshot.focused_pane_id.clone()?,
        })
    }

    fn flag_target_is_shown(&self, target: &VisitTarget) -> bool {
        self.pane_surface.as_ref().is_some_and(|surface| {
            surface.boot_id == target.boot
                && self.pane_surface_generation == target.generation
                && self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| surface.projection_revision == snapshot.revision)
                && surface
                    .panes
                    .iter()
                    .any(|pane| pane.pane_id == target.pane && pane.focused)
        })
    }

    pub(super) fn reconcile_pane_flag_visit(&mut self, now: Option<Instant>) {
        // Older endpoints never send this optional projection. Do not retain
        // an arrival (or a previous endpoint's dwell) and revisit it per frame.
        if self.presented_flag_projection().is_none() {
            self.pane_flags.observed = None;
            self.pane_flags.arrival = false;
            self.pane_flags.dwell = None;
            return;
        }
        // Surface replacement runs for every visible output frame. Compare
        // borrowed identity first, allocating only for an actual focus change.
        let same_focus = match (self.snapshot.as_deref(), self.pane_flags.observed.as_ref()) {
            (Some(snapshot), Some(observed)) => {
                observed.endpoint == self.active_endpoint_id
                    && observed.boot == snapshot.boot_id
                    && observed.generation == self.active_snapshot_generation
                    && Some(observed.pane.as_str()) == snapshot.focused_pane_id.as_deref()
            }
            (None, None) => true,
            _ => false,
        };
        if !same_focus {
            self.pane_flags.observed = self.pane_flag_target();
            self.pane_flags.arrival = self.pane_flags.observed.is_some();
            self.pane_flags.dwell = None;
        } else if !self.pane_flags.arrival && self.pane_flags.dwell.is_none() {
            return;
        }
        let Some(target) = self.pane_flags.observed.as_ref() else {
            return;
        };
        let shown = self.flag_target_is_shown(target);
        let seq = self
            .pane_flags()
            .and_then(|flags| flags.get(&target.pane))
            .map(|flag| flag.seq);
        if self
            .pane_flags
            .dwell
            .as_ref()
            .is_some_and(|dwell| seq != Some(dwell.seq))
        {
            self.pane_flags.dwell = None;
        }
        // Consume even an unflagged arrival. A later flag on this same focused
        // pane must not arm a dwell merely because a projection arrived.
        if shown && self.pane_flags.arrival && self.presented_flag_projection().is_some() {
            self.pane_flags.arrival = false;
            if let Some(seq) = seq {
                self.pane_flags.dwell = Some(Dwell {
                    target: target.clone(),
                    seq,
                    remaining: PANE_FLAG_DWELL,
                    deadline: None,
                });
            }
        }
        if self.pane_flags.dwell.is_none() {
            return;
        }
        let now = now.unwrap_or_else(|| self.pane_flags.now());
        if let Some(dwell) = &mut self.pane_flags.dwell {
            if shown && self.outer_focused != Some(false) {
                if dwell.deadline.is_none() {
                    dwell.deadline = Some(now + dwell.remaining);
                }
            } else if let Some(deadline) = dwell.deadline.take() {
                dwell.remaining = deadline.saturating_duration_since(now);
            }
        }
    }

    fn request_pane_unflag(&mut self, pane_id: String, seq: u64, outcome: &mut ClientShellInput) {
        // Unlike legacy actions, absence of method negotiation is not support.
        // Old servers must receive zero unflag requests and zero error notices.
        let supported = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| methods.contains("pane.unflag"));
        let Some(target) = self.pane_flag_target() else {
            return;
        };
        let key = (target, seq);
        if !supported
            || self
                .pane_flags
                .requested
                .values()
                .any(|pending| pending == &key)
        {
            return;
        }
        if self.push_endpoint_method_with_kind(
            Method::PaneUnflag(PaneUnflagParams {
                pane_id,
                expected_seq: Some(seq),
            }),
            PendingEndpointKind::AutomaticPaneUnflag,
            outcome,
        ) {
            if let Some(ClientShellAction::Endpoint { request, .. }) = outcome.actions.last() {
                self.pane_flags.requested.insert(request.id.clone(), key);
            }
        }
    }

    /// Suppress duplicate input clears only while their request is in flight.
    /// A failed or cancelled clear leaves server state untouched and a later
    /// key or visit must be able to try again.
    pub(super) fn pane_flag_request_finished(&mut self, request_id: &str) {
        self.pane_flags.requested.remove(request_id);
    }

    /// The endpoint activation fence can present a pane before allowing API
    /// input. Keep elapsed dwell, but stop waking for an unsendable clear.
    pub(crate) fn set_pane_flag_input_available(&mut self, available: bool) {
        self.pane_flags.input_suspended = !available;
    }

    pub(crate) fn tick_pane_flags(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        if self.pane_flags.dwell.is_none() || self.pane_flags.input_suspended {
            return;
        }
        self.reconcile_pane_flag_visit(Some(now));
        if self
            .pane_flags
            .next_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            if let Some(dwell) = self.pane_flags.dwell.take() {
                self.request_pane_unflag(dwell.target.pane, dwell.seq, outcome);
            }
        }
    }

    pub(super) fn clear_pane_flag_on_input(&mut self, outcome: &mut ClientShellInput) {
        if !self.pane_flags.any_flags {
            return;
        }
        let Some(flags) = self.pane_flags().filter(|flags| !flags.is_empty()) else {
            return;
        };
        let Some(pane) = self.focused_pane_id() else {
            return;
        };
        let Some(seq) = flags.get(&pane).map(|flag| flag.seq) else {
            return;
        };
        let forwarded = outcome.requests.iter().any(|request| matches!(request,
            ClientMessage::ClientShellPaneInput { pane_id, events } if pane_id == &pane && events.iter().any(|event| match event {
                ClientPaneInputEvent::Key { kind, .. } => !matches!(kind, crate::protocol::ClientKeyKind::Release),
                ClientPaneInputEvent::Mouse { kind, .. } => matches!(kind, crate::protocol::ClientMouseKind::Down(_)),
                ClientPaneInputEvent::TextCommit(_) | ClientPaneInputEvent::Paste(_) => true,
            })
        ));
        if forwarded {
            self.pane_flags.dwell = None;
            self.request_pane_unflag(pane, seq, outcome);
        }
    }
}
