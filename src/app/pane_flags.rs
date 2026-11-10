//! Pane flags belong to terminal identity, independently of agent detection or focus.

use crate::api::schema::PaneFlag;
use crate::app::AppState;
use crate::layout::PaneId;

impl AppState {
    pub(crate) fn set_pane_flag(
        &mut self,
        workspace: usize,
        pane: PaneId,
        message: Option<String>,
    ) -> Option<PaneFlag> {
        let id = &self
            .workspaces
            .get(workspace)?
            .pane_state(pane)?
            .attached_terminal_id;
        let terminal = self.terminals.get_mut(id)?;
        self.pane_flag_seq = self.pane_flag_seq.checked_add(1)?;
        let flag = PaneFlag {
            message: super::api_helpers::normalize_metadata_value(message),
            seq: self.pane_flag_seq,
        };
        terminal.flag = Some(flag.clone());
        self.flagged_terminals.insert(id.clone());
        Some(flag)
    }

    pub(crate) fn clear_pane_flag(
        &mut self,
        workspace: usize,
        pane: PaneId,
        expected_seq: Option<u64>,
    ) -> Option<bool> {
        let id = &self
            .workspaces
            .get(workspace)?
            .pane_state(pane)?
            .attached_terminal_id;
        let terminal = self.terminals.get_mut(id)?;
        if expected_seq.is_some_and(|seq| terminal.flag.as_ref().is_none_or(|flag| flag.seq != seq))
        {
            return Some(false);
        }
        self.flagged_terminals.remove(id);
        Some(terminal.flag.take().is_some())
    }
}

impl crate::app::App {
    /// An empty index makes ordinary render passes independent of pane count.
    /// Resolve public ids when projecting: moves may change the id's workspace
    /// prefix, but the flag continues to belong to the attached terminal.
    pub(crate) fn pane_flags_projection(
        &self,
        boot_id: &str,
    ) -> crate::protocol::endpoint::EndpointPaneFlags {
        let mut flags = Vec::new();
        if !self.state.flagged_terminals.is_empty() {
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab in &ws.tabs {
                    for (pane_id, pane) in &tab.panes {
                        if !self
                            .state
                            .flagged_terminals
                            .contains(&pane.attached_terminal_id)
                        {
                            continue;
                        }
                        if let (Some(flag), Some(pane_id)) = (
                            self.state
                                .terminals
                                .get(&pane.attached_terminal_id)
                                .and_then(|terminal| terminal.flag.clone()),
                            self.public_pane_id(ws_idx, *pane_id),
                        ) {
                            flags.push(crate::protocol::endpoint::EndpointPaneFlag {
                                pane_id,
                                flag,
                            });
                        }
                    }
                }
            }
            flags.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        }
        crate::protocol::endpoint::EndpointPaneFlags {
            boot_id: boot_id.into(),
            flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::AppState;
    use crate::workspace::Workspace;

    #[test]
    fn pane_flags_follow_moves_and_drop_on_close() {
        let mut state = AppState::test_new();
        state.workspaces = vec![Workspace::test_new("source"), Workspace::test_new("target")];
        state.active = Some(0);
        state.ensure_test_terminals();
        let pane = state.workspaces[0].focused_pane_id().unwrap();
        let terminal = state.terminal_id_for_pane(0, pane).unwrap();
        let flag = state
            .set_pane_flag(0, pane, Some("move me".into()))
            .unwrap();
        let taken = state.workspaces[0].take_pane_for_move(pane).unwrap();
        let target = state.workspaces[1].focused_pane_id().unwrap();
        state.workspaces[1]
            .insert_moved_pane_into_tab(
                0,
                target,
                taken.moved,
                ratatui::layout::Direction::Horizontal,
                0.5,
                true,
            )
            .ok()
            .unwrap();
        assert_eq!(state.terminals[&terminal].flag.as_ref(), Some(&flag));
        state.active = Some(1);
        state.close_pane();
        assert!(!state.terminals.contains_key(&terminal));
        assert!(state.flagged_terminals.is_empty());
    }

    #[test]
    fn pane_flags_set_replace_clear_and_follow_terminal() {
        let mut state = AppState::test_new();
        state.workspaces.push(Workspace::test_new("flags"));
        state.ensure_test_terminals();
        let pane = state.workspaces[0].focused_pane_id().unwrap();
        let terminal = state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        let first = state
            .set_pane_flag(0, pane, Some("  review\n ready  ".into()))
            .unwrap();
        assert_eq!(first.message.as_deref(), Some("review ready"));
        let second = state.set_pane_flag(0, pane, None).unwrap();
        assert!(second.seq > first.seq);
        assert_eq!(state.terminals[&terminal].flag.as_ref(), Some(&second));
        assert_eq!(state.clear_pane_flag(0, pane, None), Some(true));
        assert_eq!(state.clear_pane_flag(0, pane, None), Some(false));
        assert!(state.terminals[&terminal].flag.is_none());
    }
}
