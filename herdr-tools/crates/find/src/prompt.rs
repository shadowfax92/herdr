//! Query-line editing shared by every picker. Pickers own their other keys
//! (navigation, Enter, modes); typing, paste, Backspace and Ctrl-u behave the
//! same everywhere because they all come through `edit`.
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// Long enough for any realistic query, short enough that a runaway paste
/// cannot make every keystroke re-match a huge needle.
pub(crate) const MAX_QUERY: usize = 256;

/// Applies `event` to `query` and returns whether it was a deliberate edit:
/// any paste or Ctrl-u, or a Backspace/character that changed the text.
/// Pickers re-arm Enter on edits, so this matches what the user meant, not
/// only whether bytes moved. Pastes collapse whitespace onto one line and drop
/// control characters, so they can never smuggle Enter or escapes into a query.
pub(crate) fn edit(query: &mut String, event: &Event) -> bool {
    match event {
        Event::Paste(text) => {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let room = MAX_QUERY.saturating_sub(query.chars().count());
            query.extend(text.chars().filter(|c| !c.is_control()).take(room));
            true
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.clear();
                true
            }
            KeyCode::Backspace => match query.grapheme_indices(true).next_back() {
                Some((index, _)) => {
                    query.truncate(index);
                    true
                }
                None => false,
            },
            KeyCode::Char(ch)
                if key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
                    && !ch.is_control()
                    && query.chars().count() < MAX_QUERY =>
            {
                query.push(ch);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn typing_backspace_and_ctrl_u_edit_by_grapheme() {
        let mut query = String::new();
        for ch in "cafe".chars() {
            assert!(edit(
                &mut query,
                &key(KeyCode::Char(ch), KeyModifiers::NONE)
            ));
        }
        assert!(edit(&mut query, &Event::Paste("\u{301}".into())));
        assert!(edit(
            &mut query,
            &key(KeyCode::Backspace, KeyModifiers::NONE)
        ));
        assert_eq!(query, "caf");
        assert!(edit(
            &mut query,
            &key(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
        assert_eq!(query, "");
        assert!(!edit(
            &mut query,
            &key(KeyCode::Backspace, KeyModifiers::NONE)
        ));
        // Clearing an empty query is still a deliberate edit (it re-arms Enter).
        assert!(edit(
            &mut query,
            &key(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn modified_keys_and_releases_are_not_text() {
        let mut query = String::new();
        let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        for event in [
            key(KeyCode::Char('a'), KeyModifiers::ALT),
            key(KeyCode::Char('r'), KeyModifiers::CONTROL),
            key(KeyCode::Enter, KeyModifiers::NONE),
            Event::Key(release),
            Event::FocusGained,
        ] {
            assert!(!edit(&mut query, &event));
        }
        assert!(edit(
            &mut query,
            &key(KeyCode::Char('A'), KeyModifiers::SHIFT)
        ));
        assert_eq!(query, "A");
    }

    #[test]
    fn paste_is_one_line_without_controls_and_capped() {
        let mut query = String::new();
        edit(&mut query, &Event::Paste("a\tb\r\nc\u{1b}[31m".into()));
        assert_eq!(query, "a b c[31m");
        edit(&mut query, &Event::Paste("x".repeat(MAX_QUERY * 2)));
        assert_eq!(query.chars().count(), MAX_QUERY);
        assert!(!edit(
            &mut query,
            &key(KeyCode::Char('y'), KeyModifiers::NONE)
        ));
    }
}
