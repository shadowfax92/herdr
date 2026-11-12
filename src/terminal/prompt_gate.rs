//! Orders agent submissions behind the human's unsent draft. This is server-owned
//! data only: callers supply time and retain prompt text and response channels.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEventKind};

pub(crate) type PromptId = u64;
pub(crate) const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HumanInput {
    Edit,
    Clear,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentReadiness {
    Ready,
    NotReady,
    Gone,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GateAction {
    Submit(PromptId),
    Expire(PromptId),
    Undeliverable(PromptId),
}

/// One terminal's draft and FIFO. The runtime observes draft deadlines even with
/// no queued work, so clients can stop showing a hold without further input.
/// A terminal with neither a draft nor held work still needs no timer or polling.
#[derive(Debug, Clone)]
pub(crate) struct PromptGate {
    edited_at: Option<Instant>,
    clear_at: Option<Instant>,
    abandoned_after: Option<Duration>,
    enabled: bool,
    pending: VecDeque<(PromptId, Option<Instant>)>,
    #[cfg(test)]
    expiry_scans: std::cell::Cell<usize>,
}

impl Default for PromptGate {
    fn default() -> Self {
        Self {
            edited_at: None,
            clear_at: None,
            abandoned_after: Some(Duration::from_secs(600)),
            enabled: true,
            pending: VecDeque::new(),
            #[cfg(test)]
            expiry_scans: std::cell::Cell::new(0),
        }
    }
}

impl PromptGate {
    pub(crate) fn configure(&mut self, enabled: bool, abandoned_secs: u64) {
        self.enabled = enabled;
        self.abandoned_after = (abandoned_secs != 0).then(|| Duration::from_secs(abandoned_secs));
        if !enabled {
            self.edited_at = None;
            self.clear_at = None;
        }
    }

    pub(crate) fn human_input(&mut self, input: HumanInput, now: Instant) {
        if !self.enabled {
            return;
        }
        match input {
            HumanInput::Edit => {
                self.edited_at = Some(now);
                self.clear_at = None;
            }
            HumanInput::Clear if self.edited_at.is_some() => self.clear_at = Some(now + SETTLE),
            HumanInput::Clear | HumanInput::Neutral => {}
        }
    }

    pub(crate) fn push(&mut self, id: PromptId, expires_at: Option<Instant>) {
        self.pending.push_back((id, expires_at));
    }

    pub(crate) fn poll(&mut self, agent: AgentReadiness, now: Instant) -> Vec<GateAction> {
        if self
            .draft_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            self.edited_at = None;
            self.clear_at = None;
        }
        let can_submit =
            agent == AgentReadiness::Ready && (!self.enabled || self.edited_at.is_none());
        let mut actions = Vec::new();
        // Expiry must inspect the whole FIFO: a later caller's timeout can be
        // earlier than the head's, even while the draft or a dialog holds both.
        for _ in 0..self.pending.len() {
            let Some((id, expiry)) = self.pending.pop_front() else {
                break;
            };
            if expiry.is_some_and(|deadline| now >= deadline) {
                actions.push(GateAction::Expire(id));
            } else if agent == AgentReadiness::Gone {
                actions.push(GateAction::Undeliverable(id));
            } else if can_submit {
                actions.push(GateAction::Submit(id));
            } else {
                self.pending.push_back((id, expiry));
            }
        }
        actions
    }

    /// The runtime caches this scan when servicing a queue, never on the
    /// per-key loop. Draft deadlines have a separate runtime index.
    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        #[cfg(test)]
        self.expiry_scans
            .set(self.expiry_scans.get() + self.pending.len());
        self.pending.iter().filter_map(|(_, expiry)| *expiry).min()
    }

    #[cfg(test)]
    pub(crate) fn expiry_scan_count(&self) -> usize {
        self.expiry_scans.get()
    }

    pub(crate) fn held(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn holding(&self) -> bool {
        self.enabled && self.edited_at.is_some()
    }

    /// A replacement agent must never inherit the previous agent's draft or mail.
    pub(crate) fn reset(&mut self, now: Instant) -> Vec<GateAction> {
        let actions = self.poll(AgentReadiness::Gone, now);
        self.edited_at = None;
        self.clear_at = None;
        actions
    }

    pub(crate) fn draft_deadline(&self) -> Option<Instant> {
        self.clear_at
            .into_iter()
            .chain(self.edited_at.and_then(|edited| {
                self.abandoned_after
                    .and_then(|after| edited.checked_add(after))
            }))
            .min()
    }
}

/// Classify only events successfully forwarded to the PTY, never host shortcuts.
/// Modified Enter is deliberately an edit: a false clear can corrupt a draft.
/// Ctrl+C can cancel a CLI overlay while leaving its composer intact, so it is
/// also an edit; only unmodified Enter starts the settle timer.
pub(crate) fn classify(event: &crate::raw_input::RawInputEvent, blocked: bool) -> HumanInput {
    use crate::raw_input::RawInputEvent;
    if blocked {
        return HumanInput::Neutral;
    }
    match event {
        RawInputEvent::Key(key) => {
            if key.kind == KeyEventKind::Release || key.code == KeyCode::Esc {
                HumanInput::Neutral
            } else if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                HumanInput::Clear
            } else {
                HumanInput::Edit
            }
        }
        RawInputEvent::Text(text) if !text.as_str().is_empty() => HumanInput::Edit,
        RawInputEvent::Paste(text) if !text.is_empty() => HumanInput::Edit,
        _ => HumanInput::Neutral,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use HumanInput::{Clear, Edit, Neutral};

    #[test]
    fn prompt_gate_exposes_draft_lifetime_without_held_work() {
        let now = Instant::now();
        let mut gate = PromptGate::default();
        assert!(!gate.holding());
        assert_eq!(gate.draft_deadline(), None);
        gate.human_input(Edit, now);
        assert!(gate.holding());
        assert_eq!(gate.draft_deadline(), Some(now + Duration::from_secs(600)));
        gate.human_input(Clear, now);
        gate.poll(
            AgentReadiness::Ready,
            now + SETTLE - Duration::from_millis(1),
        );
        assert!(gate.holding());
        gate.poll(AgentReadiness::Ready, now + SETTLE);
        assert!(!gate.holding());
        gate.human_input(Edit, now);
        gate.poll(AgentReadiness::Ready, now + Duration::from_secs(600));
        assert!(!gate.holding());
        gate.human_input(Edit, now);
        gate.configure(false, 600);
        gate.human_input(Edit, now);
        assert!(!gate.holding());
        assert_eq!(gate.draft_deadline(), None);
        gate.configure(true, 600);
        assert!(
            !gate.holding(),
            "reenabling must not resurrect a disabled draft"
        );
    }

    #[test]
    fn prompt_gate_draft_table() {
        let cases = [
            ("empty", vec![], 0, true),
            ("edit", vec![(0, Edit)], 2000, false),
            ("settling", vec![(0, Edit), (10, Clear)], 1509, false),
            ("settled", vec![(0, Edit), (10, Clear)], 1510, true),
            (
                "rearmed",
                vec![(0, Edit), (10, Clear), (1000, Edit)],
                2000,
                false,
            ),
            ("neutral", vec![(0, Edit), (10, Neutral)], 2000, false),
            ("abandoned", vec![(0, Edit)], 600_000, true),
        ];
        let start = Instant::now();
        for (name, inputs, millis, submits) in cases {
            let mut gate = PromptGate::default();
            for (at, input) in inputs {
                gate.human_input(input, start + Duration::from_millis(at));
            }
            assert_eq!(
                gate.next_expiry(),
                None,
                "no queued mail needs no delivery deadline"
            );
            gate.push(1, None);
            let actions = gate.poll(AgentReadiness::Ready, start + Duration::from_millis(millis));
            assert_eq!(
                actions,
                if submits {
                    vec![GateAction::Submit(1)]
                } else {
                    vec![]
                },
                "{name}"
            );
            assert_eq!(gate.held(), usize::from(!submits), "{name}");
        }
    }

    #[test]
    fn prompt_gate_fifo_expiry_readiness_and_reset() {
        let now = Instant::now();
        let mut gate = PromptGate::default();
        gate.configure(true, 0);
        gate.human_input(Edit, now);
        gate.push(1, None);
        gate.push(2, Some(now));
        gate.push(3, None);
        assert_eq!(gate.next_expiry(), Some(now));
        assert_eq!(
            gate.poll(AgentReadiness::Ready, now),
            vec![GateAction::Expire(2)]
        );
        assert_eq!(gate.next_expiry(), None);
        gate.human_input(Clear, now);
        assert!(gate.poll(AgentReadiness::NotReady, now + SETTLE).is_empty());
        assert_eq!(
            gate.next_expiry(),
            None,
            "blocked panes do not spin after settle"
        );
        assert_eq!(
            gate.poll(AgentReadiness::Ready, now + SETTLE),
            vec![GateAction::Submit(1), GateAction::Submit(3)]
        );
        gate.human_input(Edit, now);
        gate.push(4, None);
        assert_eq!(gate.reset(now), vec![GateAction::Undeliverable(4)]);
        gate.push(5, None);
        assert_eq!(
            gate.poll(AgentReadiness::Ready, now),
            vec![GateAction::Submit(5)]
        );
        gate.human_input(Edit, now);
        gate.push(6, None);
        gate.configure(false, 600);
        assert_eq!(
            gate.poll(AgentReadiness::Ready, now),
            vec![GateAction::Submit(6)]
        );
    }

    #[test]
    fn prompt_gate_classification() {
        use crate::input::TerminalKey;
        use crate::raw_input::RawInputEvent;
        let cases = [
            (KeyCode::Enter, KeyModifiers::NONE, Clear),
            (KeyCode::Enter, KeyModifiers::SHIFT, Edit),
            (KeyCode::Enter, KeyModifiers::ALT, Edit),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, Edit),
            (KeyCode::Esc, KeyModifiers::NONE, Neutral),
            (KeyCode::Backspace, KeyModifiers::NONE, Edit),
        ];
        for (code, modifiers, expected) in cases {
            let event = RawInputEvent::Key(TerminalKey::new(code, modifiers));
            assert_eq!(classify(&event, false), expected);
            assert_eq!(classify(&event, true), Neutral);
        }
        assert_eq!(
            classify(
                &RawInputEvent::Key(
                    TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE)
                        .with_kind(KeyEventKind::Release)
                ),
                false
            ),
            Neutral
        );
        assert_eq!(
            classify(&RawInputEvent::Paste("draft\n".into()), false),
            Edit
        );
        assert_eq!(
            classify(
                &RawInputEvent::Text(crate::input::TextCommit::new("文")),
                false
            ),
            Edit
        );
    }
}
