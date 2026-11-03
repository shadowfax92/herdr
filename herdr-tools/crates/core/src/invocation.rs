//! What Herdr says about why this process started.
//!
//! Herdr sets `HERDR_PLUGIN_CONTEXT_JSON` (the focused workspace/tab/pane and any
//! copy-mode selection), `HERDR_PLUGIN_ACTION_ID` for actions,
//! `HERDR_PLUGIN_ENTRYPOINT_ID` for panes, and `HERDR_PLUGIN_EVENT_JSON` for event
//! hooks. Popups cannot see their caller (Herdr strips `HERDR_PANE_ID` from them),
//! so a mode that opens a popup passes the source pane on in the popup's env.

use std::ffi::OsString;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

/// `HERDR_PLUGIN_CONTEXT_JSON`. Every field is optional because Herdr omits what
/// does not apply; unknown fields are ignored so newer Herdr versions still parse.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PluginContext {
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    pub workspace_cwd: Option<String>,
    /// Git worktree membership of the focused workspace, as Herdr reports it.
    pub worktree: Option<Value>,
    pub tab_id: Option<String>,
    pub tab_label: Option<String>,
    pub focused_pane_id: Option<String>,
    pub focused_pane_cwd: Option<String>,
    pub focused_pane_agent: Option<String>,
    pub focused_pane_status: Option<String>,
    /// Copy-mode selection, for actions whose manifest `contexts` include `selection`.
    pub selected_text: Option<String>,
    pub invocation_source: Option<String>,
    pub correlation_id: Option<String>,
    pub clicked_url: Option<String>,
    pub link_handler_id: Option<String>,
}

/// One process's invocation, parsed from the environment once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Invocation {
    /// None when Herdr did not set `HERDR_PLUGIN_CONTEXT_JSON` (a shell, a test).
    pub context: Option<PluginContext>,
    pub action_id: Option<String>,
    pub entrypoint_id: Option<String>,
    /// The event payload for `[[events]]` hooks.
    pub event: Option<Value>,
}

impl Invocation {
    /// The process-wide invocation, parsed on first use. Malformed JSON is an
    /// error every time it is asked for, never a silently empty context.
    pub fn current() -> Result<&'static Invocation> {
        static CURRENT: OnceLock<Result<Invocation, String>> = OnceLock::new();
        CURRENT
            .get_or_init(|| Self::from_env().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    /// Parses the four `HERDR_PLUGIN_*` variables without caching.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var_os(key))
    }

    /// [`Invocation::from_env`] over an injected environment, for tests.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let text = |key: &str| -> Result<Option<String>> {
            lookup(key)
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| anyhow!("{key} is not valid UTF-8"))
                })
                .transpose()
        };
        let context = text("HERDR_PLUGIN_CONTEXT_JSON")?
            .map(|raw| serde_json::from_str(&raw).context("invalid HERDR_PLUGIN_CONTEXT_JSON"))
            .transpose()?;
        let event = text("HERDR_PLUGIN_EVENT_JSON")?
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| serde_json::from_str(&raw).context("invalid HERDR_PLUGIN_EVENT_JSON"))
            .transpose()?;
        let nonempty = |key| Ok::<_, anyhow::Error>(text(key)?.filter(|value| !value.is_empty()));
        Ok(Self {
            context,
            action_id: nonempty("HERDR_PLUGIN_ACTION_ID")?,
            entrypoint_id: nonempty("HERDR_PLUGIN_ENTRYPOINT_ID")?,
            event,
        })
    }

    /// An invocation carrying only `context_json`, for tests of mode logic.
    pub fn from_context_json(context_json: &str) -> Result<Self> {
        Ok(Self {
            context: Some(
                serde_json::from_str(context_json).context("invalid HERDR_PLUGIN_CONTEXT_JSON")?,
            ),
            ..Self::default()
        })
    }

    /// The pane the action was invoked on: the context's `focused_pane_id`,
    /// trimmed, non-empty and free of control characters.
    pub fn source_pane(&self) -> Result<&str> {
        let pane_id = self
            .context
            .as_ref()
            .and_then(|context| context.focused_pane_id.as_deref())
            .context("plugin context has no focused pane")?;
        valid_pane_id(pane_id).context("plugin context has no valid focused pane")
    }

    /// [`Invocation::source_pane`], else `HERDR_PANE_ID` (set when a command runs
    /// inside a Herdr pane rather than as a plugin action), as jump and layouts did.
    pub fn source_pane_or_env(&self) -> Result<String> {
        if let Ok(pane_id) = self.source_pane() {
            return Ok(pane_id.to_owned());
        }
        let pane_id =
            std::env::var("HERDR_PANE_ID").context("Herdr did not provide a source pane")?;
        Ok(valid_pane_id(&pane_id)
            .context("Herdr did not provide a valid source pane")?
            .to_owned())
    }

    /// The focused pane's cwd from the context, when Herdr sent a non-blank one.
    pub fn source_cwd(&self) -> Option<&Path> {
        self.context
            .as_ref()?
            .focused_pane_cwd
            .as_deref()
            .filter(|cwd| !cwd.trim().is_empty())
            .map(Path::new)
    }

    /// The copy-mode selection, if the action was invoked with one.
    pub fn selected_text(&self) -> Option<&str> {
        self.context.as_ref()?.selected_text.as_deref()
    }
}

fn valid_pane_id(pane_id: &str) -> Result<&str> {
    let pane_id = pane_id.trim();
    if pane_id.is_empty() || pane_id.chars().any(char::is_control) {
        bail!("invalid pane id {pane_id:?}");
    }
    Ok(pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn invocation(vars: &[(&str, &str)]) -> Result<Invocation> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect();
        Invocation::from_lookup(|key| vars.get(key).cloned())
    }

    #[test]
    fn invocation_context_requires_a_valid_focused_pane() {
        let context = Invocation::from_context_json(
            r#"{"focused_pane_id":"w2:p1","focused_pane_cwd":"/tmp/project"}"#,
        )
        .unwrap();
        assert_eq!(context.source_pane().unwrap(), "w2:p1");
        assert_eq!(context.source_cwd(), Some(Path::new("/tmp/project")));

        for bad in [
            "{}",
            r#"{"focused_pane_id":"   "}"#,
            r#"{"focused_pane_id":"w1:\np2"}"#,
        ] {
            assert!(
                Invocation::from_context_json(bad)
                    .unwrap()
                    .source_pane()
                    .is_err(),
                "{bad}"
            );
        }
        assert!(Invocation::from_context_json("not-json").is_err());
        assert_eq!(
            Invocation::from_context_json(r#"{"focused_pane_id":" w1:p2 "}"#)
                .unwrap()
                .source_pane()
                .unwrap(),
            "w1:p2"
        );
    }

    #[test]
    fn parses_every_plugin_variable_and_ignores_unknown_context_fields() {
        let invocation = invocation(&[
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"focused_pane_id":"w1:p2","selected_text":"hi","focused_pane_status":"working","future":1}"#,
            ),
            ("HERDR_PLUGIN_ACTION_ID", "comment-capture"),
            ("HERDR_PLUGIN_ENTRYPOINT_ID", ""),
            ("HERDR_PLUGIN_EVENT_JSON", r#"{"event":"pane.closed","pane_id":"w1:p3"}"#),
        ])
        .unwrap();
        assert_eq!(invocation.selected_text(), Some("hi"));
        assert_eq!(invocation.action_id.as_deref(), Some("comment-capture"));
        assert_eq!(invocation.entrypoint_id, None);
        assert_eq!(invocation.event.unwrap()["pane_id"], "w1:p3");
        assert_eq!(
            invocation.context.unwrap().focused_pane_status.as_deref(),
            Some("working")
        );
    }

    #[test]
    fn absent_context_is_empty_but_malformed_context_is_an_error() {
        let empty = invocation(&[]).unwrap();
        assert_eq!(empty, Invocation::default());
        assert!(empty.source_pane().is_err());
        assert_eq!(empty.source_cwd(), None);
        assert!(invocation(&[("HERDR_PLUGIN_CONTEXT_JSON", "{")]).is_err());
        assert!(invocation(&[("HERDR_PLUGIN_EVENT_JSON", "[")]).is_err());
    }

    #[test]
    fn blank_cwd_is_absent() {
        let context =
            Invocation::from_context_json(r#"{"focused_pane_id":"p","focused_pane_cwd":" "}"#)
                .unwrap();
        assert_eq!(context.source_cwd(), None);
    }
}
