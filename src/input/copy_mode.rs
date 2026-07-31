use crossterm::event::{KeyCode, KeyModifiers};

use super::TerminalKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyModeCommand {
    Escape,
    CopyAndExit,
    Quit,
    Left,
    Down,
    Up,
    Right,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    LineStart,
    LineEnd,
    FirstNonBlank,
    BeginSelection,
    SelectLine,
    HistoryTop,
    HistoryBottom,
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrevious,
    NextWord,
    PreviousWord,
    EndWord,
    PreviousParagraph,
    NextParagraph,
}

pub(crate) fn copy_mode_command(key: TerminalKey) -> Option<CopyModeCommand> {
    match key.code {
        KeyCode::Esc => return Some(CopyModeCommand::Escape),
        KeyCode::Enter => return Some(CopyModeCommand::CopyAndExit),
        KeyCode::Left => return Some(CopyModeCommand::Left),
        KeyCode::Down => return Some(CopyModeCommand::Down),
        KeyCode::Up => return Some(CopyModeCommand::Up),
        KeyCode::Right => return Some(CopyModeCommand::Right),
        KeyCode::PageUp => return Some(CopyModeCommand::PageUp),
        KeyCode::PageDown => return Some(CopyModeCommand::PageDown),
        KeyCode::Home => return Some(CopyModeCommand::LineStart),
        KeyCode::End => return Some(CopyModeCommand::LineEnd),
        _ => {}
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('b'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(CopyModeCommand::PageUp);
        }
        (KeyCode::Char('f'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(CopyModeCommand::PageDown);
        }
        (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(CopyModeCommand::HalfPageUp);
        }
        (KeyCode::Char('d'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(CopyModeCommand::HalfPageDown);
        }
        _ => {}
    }

    match copy_mode_text_char(key)? {
        'q' => Some(CopyModeCommand::Quit),
        'y' => Some(CopyModeCommand::CopyAndExit),
        'v' | ' ' => Some(CopyModeCommand::BeginSelection),
        'V' => Some(CopyModeCommand::SelectLine),
        'h' => Some(CopyModeCommand::Left),
        'j' => Some(CopyModeCommand::Down),
        'k' => Some(CopyModeCommand::Up),
        'l' => Some(CopyModeCommand::Right),
        'g' => Some(CopyModeCommand::HistoryTop),
        'G' => Some(CopyModeCommand::HistoryBottom),
        '0' => Some(CopyModeCommand::LineStart),
        '$' => Some(CopyModeCommand::LineEnd),
        '^' => Some(CopyModeCommand::FirstNonBlank),
        '/' => Some(CopyModeCommand::SearchForward),
        '?' => Some(CopyModeCommand::SearchBackward),
        'n' => Some(CopyModeCommand::SearchNext),
        'N' => Some(CopyModeCommand::SearchPrevious),
        'w' => Some(CopyModeCommand::NextWord),
        'b' => Some(CopyModeCommand::PreviousWord),
        'e' => Some(CopyModeCommand::EndWord),
        '{' => Some(CopyModeCommand::PreviousParagraph),
        '}' => Some(CopyModeCommand::NextParagraph),
        _ => None,
    }
}

pub(crate) fn copy_mode_text_char(key: TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }

    if let Some(ch) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(ch);
    }

    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        Some(shifted_ascii_char(ch).unwrap_or(ch))
    } else {
        Some(ch)
    }
}

pub(crate) fn shifted_ascii_char(ch: char) -> Option<char> {
    match ch {
        'a'..='z' => Some(ch.to_ascii_uppercase()),
        '1' => Some('!'),
        '2' => Some('@'),
        '3' => Some('#'),
        '4' => Some('$'),
        '5' => Some('%'),
        '6' => Some('^'),
        '7' => Some('&'),
        '8' => Some('*'),
        '9' => Some('('),
        '0' => Some(')'),
        '-' => Some('_'),
        '=' => Some('+'),
        '[' => Some('{'),
        ']' => Some('}'),
        '\\' => Some('|'),
        ';' => Some(':'),
        '\'' => Some('"'),
        ',' => Some('<'),
        '.' => Some('>'),
        '/' => Some('?'),
        '`' => Some('~'),
        _ => None,
    }
}
