use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::TerminalKey;

pub(crate) fn first_non_blank_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    for ch in text.chars() {
        if !ch.is_whitespace() {
            return Some(col);
        }
        col = col.saturating_add(char_cell_width(ch));
    }
    None
}

pub(crate) fn last_character_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    let mut last_col = None;
    for ch in text.chars() {
        let width = u16::from(crate::ghostty::unicode_codepoint_width(ch as u32));
        if width > 0 {
            last_col = Some(col);
            col = col.saturating_add(width);
        }
    }
    last_col
}

fn char_cell_width(ch: char) -> u16 {
    u16::from(crate::ghostty::unicode_codepoint_width(ch as u32)).max(1)
}

/// Which way a copy-mode page key moves. `Up` shows older output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageDirection {
    Up,
    Down,
}

/// How far a copy-mode page key asks to move. ctrl+u and ctrl+d ask for half
/// the visible pane. ctrl+b, ctrl+f, PgUp and PgDn ask for all of it, less a
/// small overlap so the lines at the edge stay on screen. Herdr's own
/// scrollback turns the unit into rows with `copy_mode_page_lines`. A
/// fullscreen app on the alternate screen gets PgUp/PgDn strokes or wheel
/// notches instead, as `client::shell::fullscreen_paging` decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageUnit {
    Half,
    Full,
}

/// Rows one page key scrolls herdr's own scrollback in a pane `height` rows
/// tall. Half is `height / 2`. Full is `height - 2`, the two-row overlap that
/// vim's ctrl+f keeps. A pane of two rows or fewer moves one row either way.
pub(crate) fn copy_mode_page_lines(height: u16, unit: PageUnit) -> usize {
    if height <= 2 {
        return 1;
    }
    match unit {
        PageUnit::Half => usize::from(height / 2),
        PageUnit::Full => usize::from(height - 2),
    }
}

pub(crate) fn copy_mode_command_char(key: TerminalKey) -> Option<char> {
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

fn shifted_ascii_char(ch: char) -> Option<char> {
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
