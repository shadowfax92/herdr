use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientGlobalMenuAction {
    Binding(crate::input::KeybindAction),
    WhatsNew,
}

pub(super) fn global_menu_attention(snapshot: &ClientShellSnapshot) -> bool {
    snapshot.update_available.is_some() || snapshot.integration_updates_available
}

pub(super) fn global_menu_item_has_badge(
    snapshot: &ClientShellSnapshot,
    action: ClientGlobalMenuAction,
) -> bool {
    (action == ClientGlobalMenuAction::WhatsNew && snapshot.update_available.is_some())
        || (action == ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Settings)
            && snapshot.integration_updates_available)
}

/// The sidebar menu's rows, shared by the desktop menu and the mobile
/// switcher. `focus_active` is the scope's focus switch (`effective_scope`):
/// the "focus on"/"focus off" row is labelled with the flip it performs.
/// Renderers and `activate_global_menu_item` address rows by index into this
/// one list, so every caller must pass the same arguments.
pub(super) fn global_menu_items(
    snapshot: &ClientShellSnapshot,
    focus_active: bool,
) -> Vec<(&'static str, ClientGlobalMenuAction)> {
    let mut items = vec![
        (
            "settings",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Settings),
        ),
        (
            "keybinds",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Help),
        ),
        (
            "reload config",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::ReloadConfig),
        ),
    ];
    if snapshot.update_available.is_some() || snapshot.latest_release_notes_available {
        items.push((
            if snapshot.update_available.is_some() {
                "update ready"
            } else {
                "what's new"
            },
            ClientGlobalMenuAction::WhatsNew,
        ));
    }
    items.extend([
        (
            "focus…",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::FocusPicker),
        ),
        (
            "hide…",
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::HidePicker),
        ),
        (
            if focus_active {
                "focus off"
            } else {
                "focus on"
            },
            ClientGlobalMenuAction::Binding(crate::input::KeybindAction::ToggleFocus),
        ),
    ]);
    items.push((
        "detach",
        ClientGlobalMenuAction::Binding(crate::input::KeybindAction::Detach),
    ));
    items
}

impl ClientShellState {
    pub(super) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::GlobalMenu(_))) {
            self.overlay = None;
        } else {
            self.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
                highlighted: 0,
            }));
        }
    }

    pub(super) fn global_menu_focus_active(&self) -> bool {
        self.effective_scope().focus.active
    }

    pub(super) fn move_global_menu_selection(&mut self, delta: isize) {
        let focus_active = self.global_menu_focus_active();
        let item_count = self
            .snapshot
            .as_deref()
            .map(|snapshot| global_menu_items(snapshot, focus_active))
            .map_or(0, |items| items.len());
        let Some(ClientShellOverlay::GlobalMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        menu.highlighted = (menu.highlighted as isize + delta)
            .clamp(0, item_count.saturating_sub(1) as isize) as usize;
    }

    pub(super) fn activate_global_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let focus_active = self.global_menu_focus_active();
        let Some(action) = self.snapshot.as_deref().and_then(|snapshot| {
            global_menu_items(snapshot, focus_active)
                .get(index)
                .map(|(_, action)| *action)
        }) else {
            return;
        };
        if action == ClientGlobalMenuAction::WhatsNew
            && self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.release_notes.as_ref())
                .is_none()
        {
            return;
        }
        self.overlay = None;
        match action {
            ClientGlobalMenuAction::Binding(binding) => {
                self.record_binding(crate::input::KeybindMatch::Action(binding), outcome)
            }
            ClientGlobalMenuAction::WhatsNew => self.open_release_notes(),
        }
        outcome.repaint = true;
    }
}
