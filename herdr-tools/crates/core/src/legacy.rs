//! Old plugin action ids and their `shadowfax.tools` replacements (DESIGN.md's
//! action-id table). `migrate-config` rewrites menu items with it, and the morning
//! cutover rewrites the owner's `[[keys.command]]` bindings with the same table,
//! so this is the one place the mapping lives.

use std::borrow::Cow;

/// `(old, new)` for every action that survives the merge.
pub const ACTION_IDS: &[(&str, &str)] = &[
    ("shadowfax.jump.open", "shadowfax.tools.find"),
    ("shadowfax.jump.paths", "shadowfax.tools.find-paths"),
    ("shadowfax.ferry.open", "shadowfax.tools.move"),
    ("shadowfax.talon.launch", "shadowfax.tools.history"),
    ("shadowfax.menu.open", "shadowfax.tools.menu"),
    (
        "shadowfax.comments.capture",
        "shadowfax.tools.comment-capture",
    ),
    (
        "shadowfax.comments.review",
        "shadowfax.tools.comment-review",
    ),
    ("shadowfax.comments.paste", "shadowfax.tools.comment-paste"),
    (
        "shadowfax.layouts.split-left-30",
        "shadowfax.tools.layout-split-left",
    ),
    (
        "shadowfax.layouts.split-right-30",
        "shadowfax.tools.layout-split-right",
    ),
    (
        "shadowfax.layouts.equalize",
        "shadowfax.tools.layout-equalize",
    ),
    ("shadowfax.layouts.grid", "shadowfax.tools.layout-grid"),
    (
        "shadowfax.scratch.toggle-nvim",
        "shadowfax.tools.scratch-nvim",
    ),
    (
        "shadowfax.scratch.toggle-shell",
        "shadowfax.tools.scratch-shell",
    ),
    ("shadowfax.scratch.reap", "shadowfax.tools.scratch-reap"),
];

/// Old installer actions with no replacement: `config.toml`'s `[[keys.command]]`
/// is now the only place keys are bound.
pub const DROPPED_ACTION_IDS: &[&str] = &[
    "shadowfax.jump.install-keybindings",
    "shadowfax.ferry.install-keybindings",
    "shadowfax.talon.install-keybindings",
    "shadowfax.menu.install",
    "shadowfax.menu.add-redistribute",
    "shadowfax.menu.add-handoff",
    "shadowfax.layouts.install-menu",
];

/// The `shadowfax.tools` action that replaces `old`, if it survived.
pub fn rewrite_action_id(old: &str) -> Option<&'static str> {
    ACTION_IDS
        .iter()
        .find(|(from, _)| *from == old)
        .map(|(_, to)| *to)
}

/// Whether `old` was an installer action that was dropped rather than renamed.
pub fn is_dropped_action_id(old: &str) -> bool {
    DROPPED_ACTION_IDS.contains(&old)
}

/// Replaces every whole old action id inside `text`, such as a menu item's
/// `herdr plugin action invoke shadowfax.scratch.reap`. An id only matches
/// between non-id characters, so `shadowfax.jump.openers` is left alone.
pub fn rewrite_action_ids(text: &str) -> Cow<'_, str> {
    let is_id_char = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
    let mut output = String::new();
    let mut copied = 0;
    let mut search = 0;
    let mut changed = false;
    while let Some(offset) = text[search..].find("shadowfax.") {
        let start = search + offset;
        let end = text[start..]
            .find(|ch: char| !is_id_char(ch))
            .map_or(text.len(), |length| start + length);
        let bounded = !text[..start].ends_with(is_id_char);
        // Herdr ids never end in '.', but prose may follow one with a full stop.
        let token = text[start..end].trim_end_matches('.');
        if let Some(new) = rewrite_action_id(token).filter(|_| bounded) {
            output.push_str(&text[copied..start]);
            output.push_str(new);
            copied = start + token.len();
            changed = true;
        }
        search = end.max(start + 1);
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    output.push_str(&text[copied..]);
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_surviving_action_maps_into_shadowfax_tools() {
        for (old, new) in ACTION_IDS {
            assert_eq!(rewrite_action_id(old), Some(*new));
            assert!(new.starts_with("shadowfax.tools."), "{new}");
            assert!(!is_dropped_action_id(old));
        }
        assert_eq!(
            rewrite_action_id("shadowfax.comments.paste"),
            Some("shadowfax.tools.comment-paste")
        );
        assert_eq!(rewrite_action_id("shadowfax.tools.find"), None);
        assert_eq!(rewrite_action_id("shadowfax.menu.install"), None);
        assert!(is_dropped_action_id("shadowfax.menu.install"));
    }

    #[test]
    fn rewrites_whole_ids_inside_commands() {
        assert_eq!(
            rewrite_action_ids(r#""$HERDR_BIN_PATH" plugin action invoke shadowfax.scratch.reap"#),
            r#""$HERDR_BIN_PATH" plugin action invoke shadowfax.tools.scratch-reap"#
        );
        assert_eq!(
            rewrite_action_ids("shadowfax.jump.open; shadowfax.jump.paths."),
            "shadowfax.tools.find; shadowfax.tools.find-paths."
        );
        for untouched in [
            "shadowfax.jump.openers",
            "xshadowfax.jump.open",
            "shadowfax.menu.install",
            "no ids here",
        ] {
            assert!(
                matches!(rewrite_action_ids(untouched), Cow::Borrowed(_)),
                "{untouched}"
            );
        }
    }
}
