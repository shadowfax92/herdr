//! The pane a layout action acts on, from tools-core's [`Invocation`].
//!
//! Actions get it from `HERDR_PLUGIN_CONTEXT_JSON`; a command typed into a Herdr
//! pane falls back to `HERDR_PANE_ID`. A popup cannot see the pane that opened
//! it, so the grid popup is handed its source explicitly (see `grid_ui::open`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use tools_core::Invocation;

#[derive(Debug, PartialEq, Eq)]
pub struct FocusedPane {
    pub pane_id: String,
    pub cwd: PathBuf,
}

impl FocusedPane {
    pub fn from_env() -> Result<Self> {
        Self::from_invocation(Invocation::current()?)
    }

    /// The invocation's focused pane, or `HERDR_PANE_ID`; its cwd, or this
    /// process's cwd when Herdr sent none.
    pub fn from_invocation(invocation: &Invocation) -> Result<Self> {
        let pane_id = invocation
            .source_pane_or_env()
            .context("plugin action has no focused pane")?;
        let cwd = match invocation.source_cwd() {
            Some(cwd) => cwd.to_path_buf(),
            None => std::env::current_dir()?,
        };

        Ok(Self { pane_id, cwd })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_focused_pane() {
        let invocation = Invocation::from_context_json(
            r#"{"focused_pane_id":"w2:p3","focused_pane_cwd":"/tmp/project"}"#,
        )
        .unwrap();

        assert_eq!(
            FocusedPane::from_invocation(&invocation).unwrap(),
            FocusedPane {
                pane_id: "w2:p3".into(),
                cwd: PathBuf::from("/tmp/project"),
            }
        );
    }
}
