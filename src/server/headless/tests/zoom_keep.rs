//! Public API zoom ownership as observed by an attached endpoint client.

use super::*;
use crate::api::schema::{ResponseResult, SuccessResponse};
use crate::layout::ZoomWidth;

fn request(server: &mut HeadlessServer, method: &str, params: serde_json::Value) -> ResponseResult {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: serde_json::from_value(serde_json::json!({
            "id": "zoom-keep", "method": method, "params": params,
        }))
        .unwrap(),
        respond_to,
        response_write_complete: None,
    });
    let response = response_rx.recv().unwrap();
    serde_json::from_str::<SuccessResponse>(&response)
        .expect(&response)
        .result
}

fn zoom_server(width: ZoomWidth) -> (HeadlessServer, crate::layout::PaneId) {
    let mut server = test_headless_server();
    let mut ws = crate::workspace::Workspace::test_new("zoom-keep");
    let owner = ws.root_pane;
    ws.test_split(ratatui::layout::Direction::Horizontal);
    ws.test_split(ratatui::layout::Direction::Vertical);
    ws.layout.focus_pane(owner);
    ws.zoomed = true;
    ws.zoom_width = width;
    server.app.state.workspaces = vec![ws];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.mode = crate::app::Mode::Terminal;
    let terminal = server.app.state.workspaces[0]
        .terminal_id(owner)
        .unwrap()
        .clone();
    server.app.terminal_runtimes.insert(
        terminal,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"OWNER"),
    );
    (server, owner)
}

#[tokio::test]
async fn attached_client_keeps_zoom_after_unfocused_api_split() {
    for width in [ZoomWidth::Fitted, ZoomWidth::Full] {
        let (mut server, owner) = zoom_server(width);
        let owner_id = server.app.public_pane_id(0, owner).unwrap();
        let (control, render) = connect_test_shell(&mut server, 9, 483, 49);
        let before = client_shell_snapshot(&control);
        let result = request(
            &mut server,
            "pane.split",
            serde_json::json!({
                "target_pane_id": owner_id, "direction": "right", "focus": false,
            }),
        );
        let ResponseResult::PaneInfo { pane } = result else {
            panic!("split response")
        };
        server.render_and_stream();
        let after = client_shell_snapshot(&control);
        assert!(after.revision > before.revision);
        assert_eq!(after.focused_pane_id.as_deref(), Some(owner_id.as_str()));
        assert!(
            after
                .tabs
                .iter()
                .find(|tab| Some(&tab.tab_id) == after.focused_tab_id.as_ref())
                .unwrap()
                .zoomed
        );
        assert_eq!(server.app.state.workspaces[0].zoom_width, width);
        let surface = recv_pane_surface(&render, "zoom after split");
        assert_eq!(surface.projection_revision, after.revision);
        assert_eq!(surface.panes.len(), 1);
        assert_eq!(surface.panes[0].pane_id, owner_id);
        let (_, new_id) = server.app.parse_pane_id(&pane.pane_id).unwrap();
        let expected = server
            .app
            .state
            .new_pane_size(crate::ui::NewPanePlacement::Existing {
                ws_idx: 0,
                pane: new_id,
            });
        let runtime = server
            .app
            .terminal_runtimes
            .get(server.app.state.workspaces[0].terminal_id(new_id).unwrap())
            .unwrap();
        assert_eq!(runtime.current_size(), expected);
        assert!(expected.0 > 0 && expected.1 > 0);
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn unfocused_api_moves_preserve_both_zooms_and_size_the_hidden_arrival() {
    for (width, alternate) in [
        (ZoomWidth::Fitted, false),
        (ZoomWidth::Full, false),
        (ZoomWidth::Fitted, true),
        (ZoomWidth::Full, true),
    ] {
        let (mut server, owner) = zoom_server(width);
        let other = server.app.state.workspaces[0]
            .layout
            .pane_ids()
            .into_iter()
            .find(|id| *id != owner)
            .unwrap();
        let mut target = crate::workspace::Workspace::test_new("target");
        let target_owner = target.root_pane;
        target.zoomed = true;
        target.zoom_width = width;
        server.app.state.workspaces.push(target);
        server.app.state.ensure_test_terminals();
        let terminal = server.app.state.workspaces[0]
            .terminal_id(other)
            .unwrap()
            .clone();
        server.app.terminal_runtimes.insert(
            terminal.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(
                900,
                400,
                if alternate {
                    b"\x1b[?1049hHIDDEN"
                } else {
                    b"HIDDEN"
                },
            ),
        );
        let pane_id = server.app.public_pane_id(0, other).unwrap();
        let tab_id = server.app.public_tab_id(1, 0).unwrap();
        let result = request(
            &mut server,
            "pane.move",
            serde_json::json!({
                "pane_id": pane_id, "focus": false,
                "destination": { "type": "tab", "tab_id": tab_id, "split": "right" },
            }),
        );
        let ResponseResult::PaneMove { move_result } = result else {
            panic!("move response")
        };
        assert!(move_result.changed);
        for (index, focus) in [(0, owner), (1, target_owner)] {
            let tab = &server.app.state.workspaces[index].tabs[0];
            assert!(tab.zoomed);
            assert_eq!(tab.zoom_width, width);
            assert_eq!(tab.layout.focused(), focus);
        }
        assert!(move_result.source_layout.unwrap().zoomed);
        assert!(move_result.target_layout.zoomed);
        let hidden_size = server
            .app
            .terminal_runtimes
            .get(&terminal)
            .unwrap()
            .current_size();
        assert!(hidden_size.0 > 0 && hidden_size.1 > 0);
        assert_ne!(hidden_size, (400, 900), "must discard the source geometry");
        server.app.state.workspaces[1].zoomed = false;
        crate::ui::resize_tab_surface(
            &server.app.state,
            &server.app.terminal_runtimes,
            1,
            0,
            server.app.state.new_pane_area(),
            server.app.state.host_cell_size,
        );
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .current_size(),
            hidden_size,
            "hidden arrival must already have its eventual visible size (alternate={alternate})"
        );
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn hidden_arrivals_use_the_destination_clients_geometry() {
    for moving in [false, true] {
        let (mut server, owner) = zoom_server(ZoomWidth::Fitted);
        let mut destination = crate::workspace::Workspace::test_new("destination");
        let destination_owner = destination.root_pane;
        destination.zoomed = true;
        server.app.state.workspaces.push(destination);
        server.app.state.ensure_test_terminals();
        let owner_id = server.app.public_pane_id(0, owner).unwrap();
        let destination_id = server.app.public_pane_id(1, destination_owner).unwrap();
        let destination_tab = server.app.public_tab_id(1, 0).unwrap();
        let (_first_control, _first_render) = connect_test_shell(&mut server, 9, 80, 23);
        let (_second_control, _second_render) = connect_test_shell(&mut server, 10, 180, 43);
        assert!(server.focus_shell_client_on_tab(10, &destination_tab));
        server.claim_shell_tab_geometry(10, false);
        server.clients.get_mut(&10).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        // The foreground context belongs to the source client, not the tab
        // receiving this unfocused move/split behind the other client's zoom.
        server.foreground_client_id = Some(9);
        server.app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 23);
        let result = if moving {
            let terminal = server.app.state.workspaces[0]
                .terminal_id(owner)
                .unwrap()
                .clone();
            server.app.terminal_runtimes.insert(
                terminal,
                crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"ARRIVAL"),
            );
            request(
                &mut server,
                "pane.move",
                serde_json::json!({
                    "pane_id": owner_id, "focus": false,
                    "destination": { "type": "tab", "tab_id": destination_tab, "split": "right" },
                }),
            )
        } else {
            request(
                &mut server,
                "pane.split",
                serde_json::json!({
                    "target_pane_id": destination_id, "focus": false, "direction": "right",
                }),
            )
        };
        let arrival = match result {
            ResponseResult::PaneMove { move_result } => move_result.pane.pane_id,
            ResponseResult::PaneInfo { pane } => pane.pane_id,
            _ => panic!("arrival response"),
        };
        let (_, arrival) = server.app.parse_pane_id(&arrival).unwrap();
        let terminal = server.app.state.workspaces[1]
            .terminal_id(arrival)
            .unwrap()
            .clone();
        let hidden_size = server
            .app
            .terminal_runtimes
            .get(&terminal)
            .unwrap()
            .current_size();
        assert!(server.app.state.workspaces[1].zoomed);
        assert_eq!(
            server.app.state.workspaces[1].layout.focused(),
            destination_owner
        );
        assert_eq!(
            hidden_size,
            (41, 87),
            "destination is 180x43, not the source's 80x23"
        );
        if moving {
            assert_eq!(
                server
                    .app
                    .terminal_runtimes
                    .get(&terminal)
                    .unwrap()
                    .pixel_size(),
                Some((870, 820))
            );
        }
        assert_eq!(
            server.app.state.view.terminal_area,
            ratatui::layout::Rect::new(0, 0, 80, 23),
            "destination sizing must not replace the foreground view"
        );
        server.app.state.workspaces[1].zoomed = false;
        server.apply_shell_tab_geometry(10, false);
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .current_size(),
            hidden_size
        );
        shutdown_test_runtimes(&mut server);
    }
}
