//! SSH inheritance at pane/tab creation. The registry chooses a remote launch;
//! Workspace still owns identity, geometry, env injection, focus, and layout.
//! Returning None preserves the stock local path without consuming its inputs.

use super::responses::{encode_error, encode_success};
use crate::{
    api::schema::{EventData, EventEnvelope, EventKind, PaneSplitParams, ResponseResult},
    app::{App, Mode},
    layout::PaneId,
    ssh_panes::InheritTarget,
};
use std::{collections::HashMap, path::PathBuf, time::Instant};

const INHERIT: &str = "HERDR_SSH_PANES_INHERIT";
const SPAWNED: &str = "HERDR_SSH_PANES_SPAWNED";

/// A remote argv launch keeps a local HOME cwd. Opt-out uses the stock shell
/// launch with the control env stripped, so it cannot accidentally reach a child.
struct LaunchPlan {
    argv: Option<Vec<String>>,
    env: Vec<(String, String)>,
}

fn launch_plan(
    target: InheritTarget,
    cwd: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<LaunchPlan, (String, String)> {
    let mut env = env.clone();
    let opt_out = env.remove(INHERIT).as_deref() == Some("0");
    let argv = if opt_out {
        None
    } else {
        let exe = crate::platform::launch_executable()
            .map_err(|e| ("ssh_panes_spawn_failed".into(), e.to_string()))?;
        let mut argv = vec![exe.display().to_string(), "ssh".into(), target.host];
        if let Some(cwd) = cwd.map(str::to_string).or(target.cwd) {
            argv.extend(["--cwd".into(), cwd]);
        }
        env.insert(SPAWNED.into(), "1".into());
        Some(argv)
    };
    Ok(LaunchPlan {
        argv,
        env: super::env::normalize_launch_env(env)?,
    })
}

fn local_home() -> Result<PathBuf, std::io::Error> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::other("HOME is not set for SSH connector launch"))
}

impl App {
    // This mirrors the stock creation tail intentionally: one early hook keeps
    // upstream rebase conflicts outside focus/event/layout policy.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ssh_panes_split(
        &mut self,
        id: &str,
        ws_idx: usize,
        target: PaneId,
        params: &PaneSplitParams,
        rows: u16,
        cols: u16,
    ) -> Option<String> {
        if !self.ssh_panes.enabled() {
            return None;
        }
        let terminal = self.state.workspaces.get(ws_idx)?.terminal_id(target)?;
        let inherited = self.ssh_panes.inherit_target(terminal, Instant::now())?;
        let plan = match launch_plan(inherited, params.cwd.as_deref(), &params.env) {
            Ok(plan) => plan,
            Err((code, message)) => return Some(encode_error(id.to_string(), &code, message)),
        };
        let cwd = if plan.argv.is_some() {
            match local_home() {
                Ok(cwd) => Some(cwd),
                Err(e) => {
                    return Some(encode_error(
                        id.to_string(),
                        "pane_split_failed",
                        e.to_string(),
                    ))
                }
            }
        } else {
            params.cwd.as_ref().map(PathBuf::from).or_else(|| {
                Some(self.resolve_new_terminal_cwd(
                    self.launch_cwd_for_pane_in_workspace(ws_idx, target),
                ))
            })
        };
        let direction = match params.direction {
            crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        let previous_focus = self.state.current_pane_focus_target();
        let default_shell = self.state.default_shell.clone();
        let shell_config = crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode);
        let ws = self.state.workspaces.get_mut(ws_idx)?;
        let scrollback = self.state.pane_scrollback_limit_bytes;
        let theme = self.state.host_terminal_theme;
        let appearance = self.state.host_terminal_appearance;
        let result = match (plan.argv.as_deref(), params.ratio) {
            (Some(argv), Some(ratio)) => ws.split_pane_argv_command_with_ratio(
                target,
                direction,
                ratio,
                rows,
                cols,
                cwd,
                argv,
                plan.env,
                scrollback,
                theme,
                appearance,
                params.focus,
            ),
            (Some(argv), None) => ws.split_pane_argv_command(
                target,
                direction,
                rows,
                cols,
                cwd,
                argv,
                plan.env,
                scrollback,
                theme,
                appearance,
                params.focus,
            ),
            (None, Some(ratio)) => ws.split_pane_with_ratio(
                target,
                direction,
                ratio,
                rows,
                cols,
                cwd,
                scrollback,
                theme,
                appearance,
                shell_config,
                plan.env,
                params.focus,
            ),
            (None, None) => ws.split_pane(
                target,
                direction,
                rows,
                cols,
                cwd,
                scrollback,
                theme,
                appearance,
                shell_config,
                plan.env,
                params.focus,
            ),
        };
        let (tab_idx, new_pane) = match result {
            Some(Ok(result)) => result,
            Some(Err(e)) => {
                return Some(encode_error(
                    id.to_string(),
                    "pane_split_failed",
                    e.to_string(),
                ))
            }
            None => {
                return Some(encode_error(
                    id.to_string(),
                    "pane_not_found",
                    "pane not found",
                ))
            }
        };
        if let Some(pane) = self.state.workspaces[ws_idx].pane_state_mut(new_pane.pane_id) {
            pane.right_click_passthrough = matches!(
                params.right_click,
                crate::api::schema::PaneRightClickTarget::Pane
            );
        }
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.mode = Mode::Terminal;
        }
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        self.schedule_session_save();
        let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) else {
            return Some(encode_error(
                id.to_string(),
                "pane_not_found",
                "new pane metadata unavailable",
            ));
        };
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated { pane: pane.clone() },
        });
        self.emit_layout_updated_event(ws_idx, tab_idx);
        Some(encode_success(
            id.to_string(),
            ResponseResult::PaneInfo { pane },
        ))
    }

    // Mirror the upstream borrowed tab-create inputs so delegation preserves all values.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ssh_panes_tab_create(
        &mut self,
        id: &str,
        ws_idx: usize,
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        focus: bool,
        label: Option<&str>,
    ) -> Option<String> {
        if cwd.is_some() || !self.ssh_panes.inherit_new_tab() {
            return None;
        }
        let ws = self.state.workspaces.get(ws_idx)?;
        let terminal = ws.terminal_id(ws.focused_pane_id()?)?;
        let inherited = self.ssh_panes.inherit_target(terminal, Instant::now())?;
        let plan = match launch_plan(inherited, None, env) {
            Ok(plan) => plan,
            Err((code, message)) => return Some(encode_error(id.to_string(), &code, message)),
        };
        let cwd = if plan.argv.is_some() {
            match local_home() {
                Ok(cwd) => cwd,
                Err(e) => {
                    return Some(encode_error(
                        id.to_string(),
                        "tab_create_failed",
                        e.to_string(),
                    ))
                }
            }
        } else {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        };
        let (rows, cols) = self.state.new_pane_size(crate::ui::NewPanePlacement::Alone);
        let default_shell = self.state.default_shell.clone();
        let ws = self.state.workspaces.get_mut(ws_idx)?;
        let result = if let Some(argv) = plan.argv {
            ws.create_tab_argv_command(
                rows,
                cols,
                cwd,
                &argv,
                plan.env,
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
            )
        } else {
            ws.create_tab(
                rows,
                cols,
                cwd,
                self.state.pane_scrollback_limit_bytes,
                self.state.host_terminal_theme,
                self.state.host_terminal_appearance,
                crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                plan.env,
            )
        };
        let (tab_idx, terminal, runtime) = match result {
            Ok(result) => result,
            Err(e) => {
                return Some(encode_error(
                    id.to_string(),
                    "tab_create_failed",
                    e.to_string(),
                ))
            }
        };
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.remove_alias_shadowed_by_new_pane(
            self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
        );
        if let Some(label) = label {
            let workspace_id = self.state.workspaces[ws_idx].id.clone();
            let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
            });
            self.state.workspaces[ws_idx].tabs[tab_idx].set_custom_name(label.into());
            crate::logging::tab_renamed(&workspace_id, &tab_id);
        }
        if focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = Mode::Terminal;
        }
        self.schedule_session_save();
        self.emit_tab_created_events(ws_idx, tab_idx);
        Some(match self.tab_created_result(ws_idx, tab_idx) {
            Some(result) => encode_success(id.to_string(), result),
            None => encode_error(
                id.to_string(),
                "tab_create_failed",
                "new tab metadata unavailable",
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_split_plan_uses_remote_paths_and_strips_control_env() {
        for (explicit, expected) in [
            (None, "/remote/job"),
            (Some("/other remote"), "/other remote"),
        ] {
            let target = InheritTarget {
                host: "studio".into(),
                cwd: Some("/remote/job".into()),
            };
            let plan = launch_plan(
                target,
                explicit,
                &HashMap::from([("KEEP".into(), "value".into())]),
            )
            .unwrap();
            let argv = plan.argv.unwrap();
            assert_eq!(&argv[1..], &["ssh", "studio", "--cwd", expected]);
            assert_eq!(
                plan.env,
                vec![
                    (SPAWNED.into(), "1".into()),
                    ("KEEP".into(), "value".into())
                ]
            );
            assert_eq!(
                local_home().unwrap(),
                PathBuf::from(std::env::var_os("HOME").unwrap())
            );
        }
        let plan = launch_plan(
            InheritTarget {
                host: "studio".into(),
                cwd: None,
            },
            None,
            &HashMap::from([
                (INHERIT.into(), "0".into()),
                ("KEEP".into(), "value".into()),
            ]),
        )
        .unwrap();
        assert!(plan.argv.is_none());
        assert_eq!(plan.env, vec![("KEEP".into(), "value".into())]);
    }
}
