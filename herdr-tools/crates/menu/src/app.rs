//! Menu navigation and recovery-screen state, independent of the terminal.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::{Config, MenuItem};
use crate::failure_ui::FailureView;
use crate::redistribute::{LayoutPlan, LayoutSnapshot};
use crate::redistribute_ui::{RedistributionInput, RedistributionState};
use crate::scope::ScopeSupport;

/// UI events cross into command execution only through these explicit outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputOutcome {
    Continue,
    Execute(MenuItem),
    ApplyLayout(Box<LayoutPlan>),
    Cancel,
}

/// Why focus, hide and the focus switch are disabled on a Herdr without scope.
/// The rows read "<label> (unavailable)" above this scrollable panel, the way
/// the retired Agents visibility row did when Agents was not running.
const SCOPE_UNAVAILABLE: &str =
    "Focus and hide unavailable: this Herdr has no scope.get / scope.pick.\n\
They are native to the herdr fork (herdr scope …); update Herdr to use them.";

/// Owns menu navigation and recovery views; process and clipboard work remain
/// in the event loop so canceling or scrolling never executes an action.
#[derive(Clone, Debug)]
pub struct MenuState {
    title: String,
    items: Vec<MenuItem>,
    warnings: Vec<String>,
    selected: Option<usize>,
    confirming: Option<usize>,
    failure: Option<String>,
    fatal_error: Option<String>,
    redistribution: Option<RedistributionState>,
    fork_failure: Option<FailureView>,
    copy_failure: Option<FailureView>,
    /// Read once as the menu opens; decides whether scope rows are enabled.
    scope: ScopeSupport,
    scope_failure: Option<FailureView>,
}

impl MenuState {
    /// A menu whose scope support is not known yet: every row is usable until
    /// [`MenuState::set_scope`] says otherwise.
    pub fn new(config: Config) -> Self {
        Self::with_scope(config, ScopeSupport::Unknown)
    }

    pub fn with_scope(config: Config, scope: ScopeSupport) -> Self {
        let mut state = Self {
            title: config.title,
            items: config.items,
            warnings: config.warnings,
            selected: None,
            confirming: None,
            failure: None,
            fatal_error: None,
            redistribution: None,
            fork_failure: None,
            copy_failure: None,
            scope: ScopeSupport::Unknown,
            scope_failure: None,
        };
        state.set_scope(scope);
        state
    }

    /// Applies what this Herdr offers for focus and hide. Without scope the
    /// scope rows are disabled and explained; the selection skips them.
    pub fn set_scope(&mut self, scope: ScopeSupport) {
        self.scope = scope;
        self.scope_failure = (scope == ScopeSupport::Missing && self.has_scope_action())
            .then(|| FailureView::new(SCOPE_UNAVAILABLE.into()));
        if !self
            .selected
            .and_then(|index| self.items.get(index))
            .is_some_and(|item| self.item_enabled(item))
        {
            self.selected = self.items.iter().position(|item| self.item_enabled(item));
        }
    }

    pub fn fatal(error: impl Into<String>) -> Self {
        Self {
            title: "commands".into(),
            items: Vec::new(),
            warnings: Vec::new(),
            selected: None,
            confirming: None,
            failure: None,
            fatal_error: Some(error.into()),
            redistribution: None,
            fork_failure: None,
            copy_failure: None,
            scope: ScopeSupport::Unknown,
            scope_failure: None,
        }
    }

    pub fn handle_event(&mut self, event: Event) -> InputOutcome {
        match event {
            Event::Key(key) => self.handle_key(key),
            _ => InputOutcome::Continue,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        if let Some(failure) = &mut self.copy_failure {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return InputOutcome::Continue;
            }
            if is_alt_q(key) {
                return InputOutcome::Cancel;
            }
            if key.code == KeyCode::Esc {
                self.copy_failure = None;
            } else if key.code == KeyCode::Enter
                && key.kind == KeyEventKind::Press
                && key.modifiers == KeyModifiers::NONE
            {
                // Clipboard failures leave the captured source intact. Retrying
                // takes a fresh snapshot; it never reuses a partially copied one.
                self.copy_failure = None;
                if let Some(index) = self.selected {
                    return self.execute(index);
                }
            } else {
                failure.handle_key(key);
            }
            return InputOutcome::Continue;
        }
        if let Some(failure) = &mut self.fork_failure {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return InputOutcome::Continue;
            }
            if is_alt_q(key) {
                return InputOutcome::Cancel;
            }
            if key.code == KeyCode::Esc {
                self.fork_failure = None;
            } else {
                failure.handle_key(key);
            }
            // A partial move invalidates the source inventory. Enter and the
            // fork accelerators cannot retry while recovery details are open.
            return InputOutcome::Continue;
        }
        if let Some(dialog) = &mut self.redistribution {
            return match dialog.handle_key(key) {
                RedistributionInput::Continue => InputOutcome::Continue,
                RedistributionInput::Back => {
                    self.redistribution = None;
                    InputOutcome::Continue
                }
                RedistributionInput::Close => InputOutcome::Cancel,
                RedistributionInput::Apply(plan) => InputOutcome::ApplyLayout(plan),
            };
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return InputOutcome::Continue;
        }
        if key.code == KeyCode::Esc || is_alt_q(key) {
            if self.confirming.take().is_some() && key.code == KeyCode::Esc {
                return InputOutcome::Continue;
            }
            return InputOutcome::Cancel;
        }
        if self.fatal_error.is_some() {
            return InputOutcome::Continue;
        }
        if matches!(
            key.code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
        ) {
            if let Some(failure) = &mut self.scope_failure {
                failure.handle_key(key);
                return InputOutcome::Continue;
            }
        }
        if let Some(index) = self.confirming {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => self.execute(index),
                KeyCode::Char('n' | 'N') => {
                    self.confirming = None;
                    InputOutcome::Continue
                }
                _ => InputOutcome::Continue,
            };
        }

        if let KeyCode::Char(character) = key.code {
            if accepts_accelerator_modifiers(key.modifiers) {
                if let Some(index) = self
                    .items
                    .iter()
                    .position(|item| item.key() == Some(character))
                {
                    return self.activate(index);
                }
                match character {
                    'j' => {
                        self.move_selection(1);
                        return InputOutcome::Continue;
                    }
                    'k' => {
                        self.move_selection(-1);
                        return InputOutcome::Continue;
                    }
                    _ => {}
                }
            }
        }

        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter => {
                if let Some(index) = self.selected {
                    return self.activate(index);
                }
            }
            _ => {}
        }
        InputOutcome::Continue
    }

    pub fn set_failure(&mut self, failure: impl Into<String>) {
        self.failure = Some(failure.into());
    }

    pub fn set_fork_failure(&mut self, failure: String) {
        self.fork_failure = Some(FailureView::new(failure));
    }

    pub fn set_copy_failure(&mut self, failure: String) {
        self.copy_failure = Some(FailureView::new(failure));
    }

    pub fn copy_failure(&self) -> Option<&FailureView> {
        self.copy_failure.as_ref()
    }

    pub fn fork_failure(&self) -> Option<&FailureView> {
        self.fork_failure.as_ref()
    }

    pub fn open_redistribution(&mut self, snapshot: LayoutSnapshot) {
        self.redistribution = Some(RedistributionState::new(snapshot));
    }

    pub fn redistribution(&self) -> Option<&RedistributionState> {
        self.redistribution.as_ref()
    }

    pub fn refresh_redistribution(&mut self, snapshot: LayoutSnapshot) {
        if let Some(dialog) = &mut self.redistribution {
            dialog.refresh(snapshot);
        }
    }

    pub fn fail_redistribution(&mut self, error: String) {
        if let Some(dialog) = &mut self.redistribution {
            dialog.fail(error);
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn confirming(&self) -> Option<usize> {
        self.confirming
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal_error.as_deref()
    }

    pub fn has_scope_action(&self) -> bool {
        self.items.iter().any(MenuItem::needs_scope)
    }

    pub fn item_enabled(&self, item: &MenuItem) -> bool {
        item.is_selectable() && !(item.needs_scope() && self.scope == ScopeSupport::Missing)
    }

    /// The panel explaining disabled scope rows, when this Herdr lacks scope.
    pub fn scope_failure(&self) -> Option<&FailureView> {
        self.scope_failure.as_ref()
    }

    /// Rendering and activation share this state. A disabled scope row keeps
    /// its configured noun phrase and says it is unavailable. Labels are
    /// otherwise the config's own: the focus switch shows no state, since the
    /// server the menu reads may not be the one whose scope the client applies.
    pub fn item_label(&self, item: &MenuItem) -> String {
        let noun = item.label().unwrap_or_default();
        if item.needs_scope() && self.scope == ScopeSupport::Missing {
            format!("{noun} (unavailable)")
        } else {
            noun.into()
        }
    }

    fn activate(&mut self, index: usize) -> InputOutcome {
        let Some(item) = self.items.get(index).filter(|item| self.item_enabled(item)) else {
            return InputOutcome::Continue;
        };
        self.selected = Some(index);
        self.failure = None;
        if item.confirmation().is_some() {
            self.confirming = Some(index);
            InputOutcome::Continue
        } else {
            self.execute(index)
        }
    }

    fn execute(&mut self, index: usize) -> InputOutcome {
        self.confirming = None;
        match self.items.get(index).filter(|item| self.item_enabled(item)) {
            Some(item) => InputOutcome::Execute(item.clone()),
            None => InputOutcome::Continue,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let selectable = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| self.item_enabled(item).then_some(index))
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.selected = None;
            return;
        }
        let position = self
            .selected
            .and_then(|selected| selectable.iter().position(|index| *index == selected))
            .unwrap_or(0);
        let next = (position as isize + delta).rem_euclid(selectable.len() as isize) as usize;
        self.selected = Some(selectable[next]);
    }
}

fn is_alt_q(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q' | 'Q')) && key.modifiers.contains(KeyModifiers::ALT)
}

fn accepts_accelerator_modifiers(modifiers: KeyModifiers) -> bool {
    modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    use super::*;
    use crate::config::Config;

    fn state(items: &str) -> MenuState {
        MenuState::new(Config::parse(items).unwrap())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_skips_separators_and_wraps() {
        let mut state = state(
            r#"
[[items]]
key = "a"
label = "alpha"
command = "alpha"
[[items]]
type = "separator"
[[items]]
key = "b"
label = "beta"
command = "beta"
"#,
        );

        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.handle_key(key(KeyCode::Down)), InputOutcome::Continue);
        assert_eq!(state.selected(), Some(2));
        state.handle_key(key(KeyCode::Down));
        assert_eq!(state.selected(), Some(0));
        state.handle_key(key(KeyCode::Up));
        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn accelerators_are_immediate_and_case_sensitive() {
        let mut state = state(
            r#"
[[items]]
key = "r"
label = "lower"
command = "lower"
[[items]]
key = "R"
label = "upper"
command = "upper"
"#,
        );

        let InputOutcome::Execute(item) = state.handle_key(key(KeyCode::Char('R'))) else {
            panic!("expected uppercase accelerator");
        };
        let MenuItem::Shell(item) = item else {
            panic!("expected shell item");
        };
        assert_eq!(item.command, "upper");
    }

    #[test]
    fn j_and_k_navigate_only_when_not_bound() {
        let mut navigation = state(
            r#"
[[items]]
key = "a"
label = "alpha"
command = "alpha"
[[items]]
key = "b"
label = "beta"
command = "beta"
"#,
        );
        navigation.handle_key(key(KeyCode::Char('j')));
        assert_eq!(navigation.selected(), Some(1));
        navigation.handle_key(key(KeyCode::Char('k')));
        assert_eq!(navigation.selected(), Some(0));

        let mut accelerator = state(
            r#"
[[items]]
key = "j"
label = "jump"
command = "jump"
[[items]]
key = "x"
label = "other"
command = "other"
"#,
        );
        let InputOutcome::Execute(item) = accelerator.handle_key(key(KeyCode::Char('j'))) else {
            panic!("expected j accelerator");
        };
        let MenuItem::Shell(item) = item else {
            panic!("expected shell item");
        };
        assert_eq!(item.command, "jump");
    }

    #[test]
    fn confirmation_accepts_and_cancels_inline() {
        let source = r#"
[[items]]
key = "x"
label = "reap"
command = "reap"
confirm = "Reap?"
"#;
        let mut accepted = state(source);

        assert_eq!(
            accepted.handle_key(key(KeyCode::Char('x'))),
            InputOutcome::Continue
        );
        assert_eq!(accepted.confirming(), Some(0));
        let InputOutcome::Execute(item) = accepted.handle_key(key(KeyCode::Enter)) else {
            panic!("expected confirmed execution");
        };
        let MenuItem::Shell(item) = item else {
            panic!("expected shell item");
        };
        assert_eq!(item.command, "reap");

        let mut cancelled = state(source);
        cancelled.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            cancelled.handle_key(key(KeyCode::Char('n'))),
            InputOutcome::Continue
        );
        assert_eq!(cancelled.confirming(), None);
    }

    #[test]
    fn escape_and_alt_q_close_but_plain_q_can_be_an_accelerator() {
        let source = r#"
[[items]]
key = "q"
label = "queue"
command = "queue"
"#;
        let mut escape = state(source);
        assert_eq!(escape.handle_key(key(KeyCode::Esc)), InputOutcome::Cancel);

        let mut alt_q = state(source);
        assert_eq!(
            alt_q.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
            InputOutcome::Cancel
        );

        let mut plain_q = state(source);
        assert!(matches!(
            plain_q.handle_key(key(KeyCode::Char('q'))),
            InputOutcome::Execute(_)
        ));
    }

    #[test]
    fn release_resize_and_unknown_keys_are_ignored() {
        let mut state = state("");
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::Enter)
        };

        assert_eq!(state.handle_key(release), InputOutcome::Continue);
        assert_eq!(
            state.handle_event(Event::Resize(100, 40)),
            InputOutcome::Continue
        );
        assert_eq!(state.handle_key(key(KeyCode::F(8))), InputOutcome::Continue);
    }

    #[test]
    fn fatal_error_keeps_only_the_close_paths() {
        let mut state = MenuState::fatal("failed to parse config");

        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            InputOutcome::Continue
        );
        assert_eq!(state.handle_key(key(KeyCode::Esc)), InputOutcome::Cancel);
        assert_eq!(state.fatal_error(), Some("failed to parse config"));
    }
}
