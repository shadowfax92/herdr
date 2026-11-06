//! The read settle: when the user reads an unread agent, its row keeps its
//! place in the priority-sorted Agents panel for `READ_SETTLE`, then moves
//! down. Without it the row the user just looked at drops below every working
//! agent at once, and the list jumps under them.
//!
//! Presentation only. `EndpointAgentPresentation` turns the agent read at once,
//! so its icon, `next_attention_agent`, notifications and counts all see it as
//! read; only the panel's priority rank is held. A hold lasts while the agent
//! stays in the state it was read in. Any real status change (working again,
//! blocked, a new unread turn) bumps its `state_change_seq` and ends the hold
//! there and then, so news is never delayed by it.
//!
//! Only a row the panel actually drew unread is held. A turn the user watches
//! finish is projected unread and read within the same snapshot and surface
//! pair, and compose never draws a snapshot without its surface, so that row
//! was never in the unread group: holding it there would add the very jump
//! this exists to remove. So the settle also remembers which agents the last
//! drawn state showed unread.
//!
//! ```text
//! acknowledge_surface ── pair drawn, drew it unread ──> hold(pane, seq, +5 s)
//!                     └─ record what this pair draws unread
//! project_snapshot ───── seq changed ─────────────────> drop the hold now
//! client loop timer ──── deadline ────────────────────> tick: drop, repaint
//! render and keys ────── panel_priority ──────────────> unread's rank if held
//! ```
//!
//! Render never reads the clock: the timer drops expired holds, and the sort
//! only asks whether a hold exists.

use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::AgentStatus;
use crate::protocol::ClientShellAgent;

/// How long a just-read agent keeps its unread rank. A constant rather than a
/// setting: it smooths one transition, and no workflow needs it longer or
/// shorter yet.
pub(super) const READ_SETTLE: Duration = Duration::from_secs(5);

/// One endpoint's agents read within the last `READ_SETTLE`, and the agents
/// its panel last drew unread. Owned by that endpoint's
/// `EndpointAgentPresentation`, because agent ids and reads are per server
/// connection.
#[derive(Clone, Debug, Default)]
pub(super) struct ReadSettle {
    // Plain vectors: `held` is the few agents read in the last seconds and is
    // scanned per sort key, `drawn_unread` is the unread agents and is scanned
    // only when an agent is read. An empty value can then be a static for
    // callers without an endpoint.
    held: Vec<HeldRead>,
    drawn_unread: Vec<String>,
    /// The snapshot revision `drawn_unread` describes, or None once a newer
    /// projection has replaced it. Lets each surface frame of an unchanged
    /// snapshot skip the rebuild.
    drawn_revision: Option<u64>,
}

#[derive(Clone, Debug)]
struct HeldRead {
    pane_id: String,
    state_change_seq: u64,
    until: Instant,
}

static NO_READ_SETTLE: ReadSettle = ReadSettle {
    held: Vec::new(),
    drawn_unread: Vec::new(),
    drawn_revision: None,
};

impl ReadSettle {
    pub(super) fn none() -> &'static Self {
        &NO_READ_SETTLE
    }

    /// Holds `agent`, which the user is reading now, at the unread rank.
    pub(super) fn hold(&mut self, agent: &ClientShellAgent, now: Instant) {
        self.held.retain(|held| held.pane_id != agent.pane_id);
        self.held.push(HeldRead {
            pane_id: agent.pane_id.clone(),
            state_change_seq: agent.state_change_seq,
            until: now.checked_add(READ_SETTLE).unwrap_or(now),
        });
    }

    /// A new snapshot was projected. Drops holds whose agent is gone or no
    /// longer in the state it was read in, so a status change moves the row
    /// with the snapshot that reports it, and marks the drawn record stale.
    pub(super) fn projected(&mut self, agents: &[ClientShellAgent]) {
        self.held
            .retain(|held| agents.iter().any(|agent| held.holds(agent)));
        self.drawn_revision = None;
    }

    /// Records which agents `snapshot` draws unread. `changed` forces a
    /// rebuild when reads changed statuses within the same revision.
    pub(super) fn note_drawn(&mut self, snapshot: &ClientShellSnapshot, changed: bool) {
        if !changed && self.drawn_revision == Some(snapshot.revision) {
            return;
        }
        self.drawn_revision = Some(snapshot.revision);
        self.drawn_unread.clear();
        self.drawn_unread.extend(
            snapshot
                .agents
                .iter()
                .filter(|agent| agent.agent_status == AgentStatus::Done)
                .map(|agent| agent.pane_id.clone()),
        );
    }

    /// Whether the last drawn state showed `pane_id` unread.
    pub(super) fn drew_unread(&self, pane_id: &str) -> bool {
        self.drawn_unread.iter().any(|drawn| drawn == pane_id)
    }

    pub(super) fn clear(&mut self) {
        self.held.clear();
        self.drawn_unread.clear();
        self.drawn_revision = None;
    }

    /// Drops holds whose time is up. True when one went, so the panel re-sorts.
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        let before = self.held.len();
        self.held.retain(|held| held.until > now);
        self.held.len() != before
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.held.iter().map(|held| held.until).min()
    }

    /// The rank the priority sort gives `agent`: its status's, or unread's
    /// while it is held. Hot: called per agent per sort, and with nothing held
    /// it is an empty scan.
    pub(super) fn panel_priority(&self, agent: &ClientShellAgent) -> u8 {
        let held = self.held.iter().any(|held| held.holds(agent));
        status_priority(if held {
            AgentStatus::Done
        } else {
            agent.agent_status
        })
    }
}

impl HeldRead {
    fn holds(&self, agent: &ClientShellAgent) -> bool {
        self.pane_id == agent.pane_id
            && self.state_change_seq == agent.state_change_seq
            && agent.agent_status == AgentStatus::Idle
    }
}

/// The read settle of `endpoint_id`'s agents, for code that has the endpoint
/// list but not the shell state, such as render.
pub(super) fn endpoint_read_settle<'a>(
    endpoints: &'a [ClientShellEndpoint],
    endpoint_id: &ClientEndpointId,
) -> &'a ReadSettle {
    match endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
    {
        Some(endpoint) => endpoint.agent_presentation.read_settle(),
        None => ReadSettle::none(),
    }
}

impl ClientShellState {
    /// The active machine's read settle, which its plain panel sorts with.
    pub(super) fn active_read_settle(&self) -> &ReadSettle {
        endpoint_read_settle(&self.endpoints, &self.active_endpoint_id)
    }

    /// Ends the holds whose time is up on every machine. True when the panel
    /// needs a repaint to move those rows.
    pub(crate) fn tick_read_settle(&mut self, now: Instant) -> bool {
        let mut expired = false;
        for endpoint in &mut self.endpoints {
            expired |= endpoint.agent_presentation.expire_read_settle(now);
        }
        expired
    }

    /// The earliest hold deadline on any machine, so the client loop wakes
    /// exactly when a row is due to move instead of polling for it.
    pub(super) fn next_read_settle_deadline(&self) -> Option<Instant> {
        self.endpoints
            .iter()
            .filter_map(|endpoint| endpoint.agent_presentation.read_settle().next_deadline())
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(status: AgentStatus, seq: u64) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: "agent-pane".into(),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            name: None,
            display_agent: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: seq,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
        }
    }

    #[test]
    fn a_held_agent_ranks_as_unread_until_its_deadline() {
        let now = Instant::now();
        let mut settle = ReadSettle::default();
        let read = agent(AgentStatus::Idle, 4);
        settle.hold(&read, now);

        assert_eq!(
            settle.panel_priority(&read),
            status_priority(AgentStatus::Done)
        );
        assert_eq!(settle.next_deadline(), Some(now + READ_SETTLE));
        assert!(!settle.expire(now + READ_SETTLE - Duration::from_millis(1)));
        assert!(settle.expire(now + READ_SETTLE));
        assert_eq!(
            settle.panel_priority(&read),
            status_priority(AgentStatus::Idle)
        );
        assert_eq!(settle.next_deadline(), None);
    }

    #[test]
    fn a_state_change_ends_the_hold() {
        let now = Instant::now();
        let mut settle = ReadSettle::default();
        settle.hold(&agent(AgentStatus::Idle, 4), now);

        let working = agent(AgentStatus::Working, 5);
        assert_eq!(
            settle.panel_priority(&working),
            status_priority(AgentStatus::Working)
        );
        settle.projected(std::slice::from_ref(&working));
        assert_eq!(settle.next_deadline(), None);
    }
}
