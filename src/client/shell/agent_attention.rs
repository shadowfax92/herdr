//! `next_attention_agent`: one key that walks the Agents panel, across every
//! online machine, to the next agent whose status the user subscribes to in
//! `[ui.sidebar.agents].attention` (blocked and unread by default).
//!
//! Statuses are the panel's own, so `done` is the client's per-pane unread mark
//! from `endpoint_agent_state`, not the server's per-tab one. Order is the
//! panel's order: with the priority sort, blocked agents come first and then
//! unread ones, each newest first. The focused agent is the cursor, so repeated
//! presses keep moving down the panel and wrap, instead of returning to an
//! agent that stays blocked until it is answered.

use super::aggregate_navigation::{
    aggregate_agent_rows, in_hidden_workspace, AggregateAgentRow, AggregateAgentTarget,
};
use super::*;

/// The first row after `focused`, wrapping, whose status is listed in
/// `attention`. The focused row itself never qualifies, so None means there is
/// nowhere else to go.
pub(super) fn next_attention_target(
    rows: &[AggregateAgentRow<'_>],
    focused: Option<(&ClientEndpointId, &str)>,
    attention: &[String],
) -> Option<AggregateAgentTarget> {
    let current = focused.and_then(|(endpoint_id, pane_id)| {
        rows.iter()
            .position(|row| row.endpoint.endpoint_id == endpoint_id && row.agent.pane_id == pane_id)
    });
    let start = current.map_or(0, |index| index + 1);
    (0..rows.len())
        .map(|offset| (start + offset) % rows.len())
        .filter(|index| Some(*index) != current)
        .map(|index| &rows[index])
        .find(|row| {
            let status = status_text(row.agent.agent_status);
            attention.iter().any(|wanted| wanted == status)
        })
        .map(|row| AggregateAgentTarget {
            endpoint_id: row.endpoint.endpoint_id.clone(),
            pane_id: row.agent.pane_id.clone(),
        })
}

impl ClientShellState {
    /// Focuses the next agent needing attention, switching machine when it
    /// lives on another one, or shows a notice when there is none.
    pub(super) fn focus_next_attention_agent(&mut self, outcome: &mut ClientShellInput) {
        let focused_pane = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.clone());
        // The rows the panel draws: online machines only, hidden workspaces out.
        let target = {
            let rows = aggregate_agent_rows(
                &self.endpoints,
                &self.active_endpoint_id,
                self.config.agent_panel_sort,
            )
            .into_iter()
            .filter(|row| !row.endpoint.stale() && !in_hidden_workspace(row, &self.config.agents))
            .collect::<Vec<_>>();
            next_attention_target(
                &rows,
                focused_pane
                    .as_deref()
                    .map(|pane_id| (&self.active_endpoint_id, pane_id)),
                &self.config.agents.attention,
            )
        };
        let Some(target) = target else {
            self.show_no_attention_notice();
            outcome.repaint = true;
            return;
        };
        let single_machine_index = (!self.multi_endpoint_active())
            .then(|| {
                let snapshot = self.snapshot.as_deref()?;
                super::agent_sidebar::listed_agent_pane_ids(snapshot, &self.config)
                    .iter()
                    .position(|pane_id| pane_id == &target.pane_id)
            })
            .flatten();
        if !self.focus_or_activate(
            target.endpoint_id.clone(),
            ClientEndpointFocusTarget::Pane(target.pane_id.clone()),
            outcome,
        ) {
            return;
        }
        // Scroll the panel to the target, mirroring next_agent: one machine
        // draws a plain list, several draw the aggregate list, and a target on
        // another machine is revealed once that machine becomes active.
        if let Some(index) = single_machine_index {
            if !self
                .hits
                .agents
                .iter()
                .any(|(_, visible_pane_id)| visible_pane_id == &target.pane_id)
            {
                self.agent_scroll = index.min(self.hits.agent_max_scroll);
            }
        } else if target.endpoint_id == self.active_endpoint_id {
            self.reveal_endpoint_agent(
                &target.endpoint_id,
                &target.pane_id,
                self.hits.agent_body.height,
            );
        } else {
            self.pending_agent_reveal = Some((target.endpoint_id, target.pane_id));
        }
        outcome.repaint = true;
    }

    fn show_no_attention_notice(&mut self) {
        const CODE: &str = "no-attention-agent";
        // A press must not hide another notice, such as a machine diagnostic.
        if self
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.key.code != CODE)
        {
            return;
        }
        let statuses = self
            .config
            .agents
            .attention
            .iter()
            .map(|status| if status == "done" { "unread" } else { status })
            .collect::<Vec<_>>();
        let listed = match statuses.split_last() {
            Some((last, [])) => (*last).to_owned(),
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
            None => "matching".to_owned(),
        };
        self.visible_endpoint_notice = Some(ClientVisibleEndpointNotice {
            key: ClientEndpointNoticeKey {
                boot_id: "attention".into(),
                kind: ClientEndpointNoticeKind::Unavailable,
                code: CODE.into(),
            },
            title: format!("No other {listed} agents"),
            body: String::new(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
        });
    }
}
