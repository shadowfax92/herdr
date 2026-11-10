//! Runtime orchestration for PromptGate. Text and callers live here; the pure
//! terminal gate owns only draft timing and FIFO ids. All decisions and PTY
//! enqueue operations run synchronously on the server loop, before another
//! human event can enter the actor queue.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::Instant;

use super::api::agents::ValidatedAgentPrompt;
use super::api::responses::{encode_error, encode_success};
use super::App;
use crate::api::schema::{AgentPromptParams, NotificationShowParams, ResponseResult};
use crate::terminal::prompt_gate::{AgentReadiness, GateAction, HumanInput, PromptId};
use crate::terminal::TerminalId;

struct PendingPrompt {
    validated: ValidatedAgentPrompt,
    params: AgentPromptParams,
    respond_to: Option<Sender<String>>,
    was_held: bool,
}

/// Pending caller resources are deliberately absent from persisted terminal data.
#[derive(Default)]
pub(crate) struct PromptDelivery {
    next_id: PromptId,
    pending: HashMap<PromptId, PendingPrompt>,
    terminals: HashSet<TerminalId>,
    pub(crate) notifications: Vec<NotificationShowParams>,
}

impl App {
    pub(crate) fn configure_prompt_gate(&mut self, terminal_id: &TerminalId) {
        let config = self.state.agent_prompt_config;
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal
                .prompt_gate
                .configure(config.hold_while_drafting, config.abandoned_draft_secs);
        }
    }

    pub(crate) fn note_prompt_human_input(
        &mut self,
        terminal_id: &TerminalId,
        inputs: &[HumanInput],
        now: Instant,
    ) {
        self.configure_prompt_gate(terminal_id);
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            if terminal.state != crate::detect::AgentState::Blocked {
                for input in inputs {
                    terminal.prompt_gate.human_input(*input, now);
                }
            }
        }
        self.poll_prompt_gate(terminal_id, now);
    }

    pub(crate) fn accept_agent_prompt(
        &mut self,
        validated: ValidatedAgentPrompt,
        params: AgentPromptParams,
        respond_to: Sender<String>,
    ) {
        let now = Instant::now();
        let Some(terminal_id) = self.state.workspaces[validated.ws_idx]
            .terminal_id(validated.pane_id)
            .cloned()
        else {
            let _ = respond_to.send(encode_error(
                validated.id,
                "agent_not_found",
                "agent terminal disappeared",
            ));
            return;
        };
        self.configure_prompt_gate(&terminal_id);
        self.prompt_delivery.next_id += 1;
        let id = self.prompt_delivery.next_id;
        let expiry = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            let _ = respond_to.send(encode_error(
                validated.id,
                "agent_not_found",
                "agent terminal disappeared",
            ));
            return;
        };
        terminal.prompt_gate.push(id, expiry);
        self.prompt_delivery.terminals.insert(terminal_id.clone());
        self.prompt_delivery.pending.insert(
            id,
            PendingPrompt {
                validated,
                params,
                respond_to: Some(respond_to),
                was_held: false,
            },
        );
        self.poll_prompt_gate(&terminal_id, now);
        if let Some(prompt) = self.prompt_delivery.pending.get_mut(&id) {
            prompt.was_held = true;
            let target = prompt
                .validated
                .agent
                .name
                .as_deref()
                .unwrap_or(&prompt.validated.agent.pane_id);
            tracing::info!(prompt_id = id, %terminal_id, target, "agent prompt held behind human draft");
            self.prompt_delivery.notifications.push(notification(
                format!("Prompt held for {target}"),
                prompt
                    .params
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            ));
            if prompt.params.wait.is_none() {
                let mut agent = prompt.validated.agent.clone();
                agent.held_prompts = self
                    .state
                    .terminals
                    .get(&terminal_id)
                    .map(|terminal| terminal.prompt_gate.held());
                if let Some(respond_to) = prompt.respond_to.take() {
                    let _ = respond_to.send(encode_success(
                        prompt.validated.id.clone(),
                        ResponseResult::AgentPrompted {
                            agent,
                            delivery: Some("held".to_owned()),
                        },
                    ));
                }
            }
        }
    }

    pub(crate) fn poll_prompt_gate(&mut self, terminal_id: &TerminalId, now: Instant) {
        if !self.prompt_delivery.terminals.contains(terminal_id) {
            return;
        }
        let Some(terminal) = self.state.terminals.get_mut(terminal_id) else {
            self.discard_prompt_gate(terminal_id, "agent terminal disappeared");
            return;
        };
        let readiness = if terminal.effective_known_agent().is_none() {
            AgentReadiness::Gone
        } else if terminal.state == crate::detect::AgentState::Blocked
            || terminal.managed_agent_launch_pending()
        {
            AgentReadiness::NotReady
        } else {
            AgentReadiness::Ready
        };
        let actions = terminal.prompt_gate.poll(readiness, now);
        let empty = terminal.prompt_gate.held() == 0;
        if empty {
            self.prompt_delivery.terminals.remove(terminal_id);
        }
        for action in actions {
            self.apply_prompt_action(terminal_id, action);
        }
    }

    fn apply_prompt_action(&mut self, terminal_id: &TerminalId, action: GateAction) {
        let id = match action {
            GateAction::Submit(id) | GateAction::Expire(id) | GateAction::Undeliverable(id) => id,
        };
        let Some(mut prompt) = self.prompt_delivery.pending.remove(&id) else {
            return;
        };
        match action {
            GateAction::Expire(_) => {
                if let Some(respond_to) = prompt.respond_to {
                    let _ = respond_to.send(encode_error(
                        prompt.validated.id,
                        "timeout",
                        "prompt timed out while held behind a human draft; nothing was typed",
                    ));
                }
            }
            GateAction::Undeliverable(_) => {
                self.fail_held_prompt(prompt, "agent went away while prompt was held")
            }
            GateAction::Submit(_) => {
                if prompt.was_held {
                    // Re-resolve by terminal identity, so a move/rename cannot send
                    // held text to the terminal now occupying its old pane position.
                    let target = self
                        .resolve_terminal_target(terminal_id.as_str())
                        .ok()
                        .and_then(|target| self.public_pane_id(target.ws_idx, target.pane_id));
                    let Some(target) = target else {
                        self.fail_held_prompt(prompt, "agent terminal is no longer attached");
                        return;
                    };
                    prompt.params.target = target;
                    match self.validate_agent_prompt(prompt.validated.id.clone(), &prompt.params) {
                        Ok(validated)
                            if validated.expected_agent == prompt.validated.expected_agent =>
                        {
                            prompt.validated = validated
                        }
                        _ => {
                            self.fail_held_prompt(
                                prompt,
                                "agent changed or disappeared while prompt was held",
                            );
                            return;
                        }
                    }
                    tracing::info!(prompt_id = id, %terminal_id, "releasing held agent prompt");
                }
                let owned = prompt.respond_to.is_none().then(|| {
                    (
                        prompt
                            .validated
                            .agent
                            .name
                            .clone()
                            .unwrap_or_else(|| prompt.params.target.clone()),
                        prompt.params.text.clone(),
                    )
                });
                let respond_to = prompt.respond_to.unwrap_or_else(|| {
                    let (tx, _) = std::sync::mpsc::channel();
                    tx
                });
                self.start_agent_prompt_submission(
                    prompt.validated,
                    &prompt.params,
                    respond_to,
                    owned,
                );
            }
        }
    }

    fn fail_held_prompt(&mut self, prompt: PendingPrompt, reason: &str) {
        let target = prompt
            .validated
            .agent
            .name
            .as_deref()
            .unwrap_or(&prompt.params.target);
        self.report_held_prompt_failure(target.to_owned(), prompt.params.text, reason);
        if let Some(respond_to) = prompt.respond_to {
            let _ = respond_to.send(encode_error(prompt.validated.id, "agent_not_found", reason));
        }
    }

    pub(crate) fn report_held_prompt_failure(
        &mut self,
        target: String,
        text: String,
        reason: &str,
    ) {
        tracing::warn!(
            target,
            text,
            reason,
            "held agent prompt could not be delivered"
        );
        self.prompt_delivery.notifications.push(notification(
            format!("Prompt undeliverable for {target}"),
            format!("{reason}: {text}"),
        ));
    }

    pub(crate) fn reset_prompt_gate(&mut self, terminal_id: &TerminalId, now: Instant) {
        let actions = self
            .state
            .terminals
            .get_mut(terminal_id)
            .map(|terminal| terminal.prompt_gate.reset(now))
            .unwrap_or_default();
        self.prompt_delivery.terminals.remove(terminal_id);
        for action in actions {
            self.apply_prompt_action(terminal_id, action);
        }
    }

    pub(crate) fn discard_prompt_gate(&mut self, terminal_id: &TerminalId, reason: &str) {
        self.reset_prompt_gate(terminal_id, Instant::now());
        let ids = self
            .prompt_delivery
            .pending
            .iter()
            .filter_map(|(id, prompt)| {
                (prompt.validated.agent.terminal_id == terminal_id.as_str()).then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(prompt) = self.prompt_delivery.pending.remove(&id) {
                self.fail_held_prompt(prompt, reason);
            }
        }
    }

    pub(crate) fn discard_all_prompt_gates(&mut self, reason: &str) {
        let terminals = self
            .prompt_delivery
            .terminals
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for terminal_id in terminals {
            self.discard_prompt_gate(&terminal_id, reason);
        }
    }

    pub(crate) fn next_prompt_gate_deadline(&self) -> Option<Instant> {
        self.prompt_delivery
            .terminals
            .iter()
            .filter_map(|id| self.state.terminals.get(id)?.prompt_gate.next_deadline())
            .min()
    }

    pub(crate) fn poll_due_prompt_gates(&mut self, now: Instant) {
        let due = self
            .prompt_delivery
            .terminals
            .iter()
            .filter(|id| {
                self.state
                    .terminals
                    .get(*id)
                    .and_then(|terminal| terminal.prompt_gate.next_deadline())
                    .is_some_and(|deadline| deadline <= now)
            })
            .cloned()
            .collect::<Vec<_>>();
        for id in due {
            self.poll_prompt_gate(&id, now);
        }
    }
}

fn notification(title: String, body: String) -> NotificationShowParams {
    NotificationShowParams {
        title,
        body: Some(body),
        position: None,
        sound: Default::default(),
    }
}
