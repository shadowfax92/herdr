//! Focus and hide on the headless server: the scope projection reaches every
//! attached shell on attach and on change, the client lane may change it with
//! its surface off, and `scope.pick` and `scope.toggle` go to one client only.
use super::*;

use crate::scope::{ScopePickMode, ScopeSetParams, ScopeState, ScopeTarget};

/// A test client's receivers. The render receiver must outlive the client: the
/// writer's delivery thread stops at the first frame it cannot hand over.
struct TestShell {
    control: std::sync::mpsc::Receiver<Vec<u8>>,
    _render: std::sync::mpsc::Receiver<Vec<u8>>,
}

fn attach(server: &mut HeadlessServer, client_id: u64, surface_active: bool) -> TestShell {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            prompt_holds: false,
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active,
            writer,
        })
    );
    TestShell {
        control,
        _render: render,
    }
}

/// Every endpoint control sent so far, in order. The writer delivers on its own
/// thread, so this waits until the channel has been quiet for a moment.
fn drain_controls(shell: &TestShell) -> Vec<(String, String)> {
    let mut controls = Vec::new();
    while let Ok(bytes) = shell.control.recv_timeout(Duration::from_millis(150)) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            controls.push((kind, data));
        }
    }
    controls
}

fn scope_projections(
    controls: &[(String, String)],
) -> Vec<protocol::endpoint::EndpointScopeProjection> {
    controls
        .iter()
        .filter(|(kind, _)| kind == protocol::endpoint::SCOPE_PROJECTION_KIND)
        .map(|(_, data)| serde_json::from_str(data).unwrap())
        .collect()
}

fn api(server: &mut HeadlessServer, method: api::schema::Method) -> (bool, serde_json::Value) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "scope".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
    });
    let response = response_rx.recv().expect("api response");
    (changed, serde_json::from_str(&response).unwrap())
}

fn focus_on(label: &str) -> api::schema::Method {
    api::schema::Method::ScopeSet(ScopeSetParams {
        focus_targets: Some(vec![ScopeTarget::parse(label).unwrap()]),
        focus_active: Some(true),
        hidden: None,
    })
}

#[tokio::test]
async fn attach_sends_the_scope_before_anything_else() {
    let mut server = test_headless_server();
    server.app.state.scope = ScopeState::from_config_hidden(&["ft".into()]);
    let shell = attach(&mut server, 41, true);

    let controls = drain_controls(&shell);
    let kinds: Vec<&str> = controls.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds[..4],
        [
            protocol::endpoint::SCOPE_PROJECTION_KIND,
            protocol::endpoint::PANE_FLAGS_KIND,
            protocol::endpoint::AGENT_COMPLETIONS_KIND,
            protocol::endpoint::ENDPOINT_SNAPSHOT_KIND,
        ]
    );
    let projection = &scope_projections(&controls)[0];
    assert_eq!(projection.boot_id, "test-boot");
    assert_eq!(projection.revision, 1);
    assert_eq!(projection.scope, server.app.state.scope);
}

#[tokio::test]
async fn a_scope_change_reaches_every_shell_including_inactive_ones() {
    let mut server = test_headless_server();
    let active = attach(&mut server, 42, true);
    let inactive = attach(&mut server, 43, false);
    server.render_and_stream();
    drain_controls(&active);
    drain_controls(&inactive);

    let (changed, response) = api(&mut server, focus_on("elsewhere"));
    assert!(changed, "scope.set must schedule a render");
    assert_eq!(response["result"]["type"], "scope");
    server.render_and_stream();
    for shell in [&active, &inactive] {
        let controls = drain_controls(shell);
        let projections = scope_projections(&controls);
        assert_eq!(projections.len(), 1, "{controls:?}");
        assert_eq!(projections[0].scope, server.app.state.scope);
        // Ahead of any snapshot sent in the same pass.
        assert_eq!(controls[0].0, protocol::endpoint::SCOPE_PROJECTION_KIND);
    }

    // On a settled server, a scope change alone sends only the scope; an
    // unchanged scope is never resent.
    server.render_and_stream();
    drain_controls(&active);
    drain_controls(&inactive);
    server.app.state.scope.focus.active = false;
    server.render_and_stream();
    for shell in [&active, &inactive] {
        let controls = drain_controls(shell);
        let kinds: Vec<&str> = controls.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, [protocol::endpoint::SCOPE_PROJECTION_KIND]);
        assert!(!scope_projections(&controls)[0].scope.focus.active);
    }
    server.render_and_stream();
    assert!(scope_projections(&drain_controls(&active)).is_empty());
}

#[tokio::test]
async fn the_client_lane_changes_the_scope_with_its_surface_off() {
    let mut server = test_headless_server();
    let shell = attach(&mut server, 44, false);
    drain_controls(&shell);
    let boot_id = server.client_shell_boot_id.clone();

    let api::schema::Method::ScopeSet(params) = focus_on("elsewhere") else {
        unreachable!();
    };
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 44,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "client-shell:9".into(),
                method: api::schema::Method::ScopeSet(params),
            }),
        })
    );
    let ServerMessage::ClientShellEndpointResponseChunk {
        boot_id: response_boot,
        request_id,
        final_chunk,
        data,
    } = read_server_message(shell.control.recv().expect("scope response"))
    else {
        panic!("expected endpoint response");
    };
    assert_eq!(
        (response_boot.as_str(), request_id.as_str()),
        (boot_id.as_str(), "client-shell:9")
    );
    assert!(final_chunk);
    let response: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(response["id"], "client-shell:9");
    assert_eq!(response["result"]["type"], "scope");
    assert!(server.app.state.scope.focus_engaged());

    // Other methods on that lane still need an active surface.
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 44,
            boot_id,
            request: Box::new(api::schema::Request {
                id: "client-shell:10".into(),
                method: api::schema::Method::ProductAnnouncementDismiss(
                    api::schema::ProductAnnouncementDismissParams {
                        version: "0".into(),
                        id: "none".into(),
                    },
                ),
            }),
        })
    );
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } =
        read_server_message(shell.control.recv().expect("rejection"))
    else {
        panic!("expected endpoint response");
    };
    let error: api::schema::ErrorResponse = serde_json::from_slice(&data).unwrap();
    assert_eq!(error.error.code, "surface_inactive");
}

#[tokio::test]
async fn pick_goes_to_the_foreground_client_only() {
    let mut server = test_headless_server();
    let (_, response) = api(
        &mut server,
        api::schema::Method::ScopePick(crate::scope::ScopePickParams::default()),
    );
    assert_eq!(response["error"]["code"], "no_client");

    let background_rx = attach(&mut server, 45, true);
    let foreground_rx = attach(&mut server, 46, true);
    assert_eq!(server.foreground_client_id, Some(46));
    drain_controls(&background_rx);
    drain_controls(&foreground_rx);

    let (changed, response) = api(
        &mut server,
        api::schema::Method::ScopePick(crate::scope::ScopePickParams {
            mode: ScopePickMode::Hide,
        }),
    );
    assert!(!changed);
    assert_eq!(response["result"]["type"], "ok");
    let picks = |receiver| -> Vec<protocol::endpoint::EndpointScopePick> {
        drain_controls(receiver)
            .into_iter()
            .filter(|(kind, _)| kind == protocol::endpoint::SCOPE_PICK_KIND)
            .map(|(_, data)| serde_json::from_str(&data).unwrap())
            .collect()
    };
    assert_eq!(
        picks(&foreground_rx),
        [protocol::endpoint::EndpointScopePick {
            mode: ScopePickMode::Hide
        }]
    );
    assert!(picks(&background_rx).is_empty());
}

#[tokio::test]
async fn toggle_goes_to_one_client_whose_scope_set_flips_focus() {
    let mut server = test_headless_server();
    let (_, response) = api(
        &mut server,
        api::schema::Method::ScopeToggle(api::schema::EmptyParams::default()),
    );
    assert_eq!(response["error"]["code"], "no_client");

    let background = attach(&mut server, 47, true);
    let foreground = attach(&mut server, 48, true);
    assert_eq!(server.foreground_client_id, Some(48));
    api(&mut server, focus_on("elsewhere"));
    server.render_and_stream();
    drain_controls(&background);
    drain_controls(&foreground);

    // The server only relays the toggle: the scope stays as it was until the
    // client answers.
    let (changed, response) = api(
        &mut server,
        api::schema::Method::ScopeToggle(api::schema::EmptyParams::default()),
    );
    assert!(!changed);
    assert_eq!(response["result"]["type"], "ok");
    assert!(server.app.state.scope.focus.active);
    let toggles = |shell| {
        drain_controls(shell)
            .into_iter()
            .filter(|(kind, _)| kind == protocol::endpoint::SCOPE_TOGGLE_KIND)
            .count()
    };
    assert_eq!(toggles(&foreground), 1);
    assert_eq!(toggles(&background), 0);

    // The client's answer: its toggle path sends the flipped flag on the
    // client lane, and every shell sees the new scope.
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 48,
            boot_id,
            request: Box::new(api::schema::Request {
                id: "client-shell:1".into(),
                method: api::schema::Method::ScopeSet(ScopeSetParams {
                    focus_active: Some(false),
                    ..ScopeSetParams::default()
                }),
            }),
        })
    );
    server.render_and_stream();
    for shell in [&foreground, &background] {
        let projections = scope_projections(&drain_controls(shell));
        assert_eq!(projections.len(), 1);
        assert!(!projections[0].scope.focus.active);
        assert_eq!(projections[0].scope.focus.targets.len(), 1);
    }
}

mod mirror {
    //! `scope.mirror`: the client hands this (remote) server the scope it
    //! filters by, and plugin `scope.get` judges local tabs by it, as the client
    //! names this machine, while that client is the one acting.
    use super::*;

    use crate::scope::{ScopeFocus, ScopeMirrorParams};

    const STUDIO_KEY: &str = "ssh:0123456789abcdef0123456789abcdef";

    /// `main` (tabs `fish`, `outreach`) and `admin` (tab `shell`), with this
    /// server's own scope hiding `admin`, so a reply judged by the server's
    /// own state is easy to tell from one judged by a mirror.
    fn studio_server() -> HeadlessServer {
        let mut server = test_headless_server();
        let mut main = crate::workspace::Workspace::test_new("main");
        main.tabs[0].custom_name = Some("fish".into());
        main.test_add_tab(Some("outreach"));
        let mut admin = crate::workspace::Workspace::test_new("admin");
        admin.tabs[0].custom_name = Some("shell".into());
        server.app.state.workspaces = vec![main, admin];
        server.app.state.scope = ScopeState {
            hidden: vec![ScopeTarget::parse("admin").unwrap()],
            ..ScopeState::default()
        };
        server
    }

    fn focus(targets: Vec<ScopeTarget>) -> ScopeState {
        ScopeState {
            focus: ScopeFocus {
                active: true,
                targets,
            },
            hidden: Vec::new(),
        }
    }

    fn target(text: &str) -> ScopeTarget {
        ScopeTarget::parse(text).unwrap()
    }

    /// One client-lane request and its response, skipping any projection the
    /// writer delivers first.
    fn lane(
        server: &mut HeadlessServer,
        shell: &TestShell,
        client_id: u64,
        method: api::schema::Method,
    ) -> serde_json::Value {
        let boot_id = server.client_shell_boot_id.clone();
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request: Box::new(api::schema::Request {
                id: "client-shell:1".into(),
                method,
            }),
        });
        loop {
            let message = read_server_message(
                shell
                    .control
                    .recv_timeout(Duration::from_secs(1))
                    .expect("endpoint response"),
            );
            if let ServerMessage::ClientShellEndpointResponseChunk { data, .. } = message {
                return serde_json::from_slice(&data).unwrap();
            }
        }
    }

    fn mirror(
        server: &mut HeadlessServer,
        shell: &TestShell,
        client_id: u64,
        scope: Option<ScopeState>,
    ) {
        let response = lane(
            server,
            shell,
            client_id,
            api::schema::Method::ScopeMirror(ScopeMirrorParams {
                scope,
                machine: STUDIO_KEY.into(),
                machine_label: "studio".into(),
            }),
        );
        assert_eq!(response["result"]["type"], "ok", "{response}");
    }

    /// `workspace/tab` labels of the tabs a `scope.get` reply admits.
    fn admitted(server: &HeadlessServer, response: &serde_json::Value) -> Vec<String> {
        assert_eq!(response["result"]["type"], "scope", "{response}");
        let mut labels = HashMap::new();
        for (ws_idx, ws) in server.app.state.workspaces.iter().enumerate() {
            for tab_idx in 0..ws.tabs.len() {
                labels.insert(
                    server.app.public_tab_id(ws_idx, tab_idx).unwrap(),
                    format!(
                        "{}/{}",
                        ws.custom_name.as_deref().unwrap(),
                        ws.tab_display_name(tab_idx).unwrap()
                    ),
                );
            }
        }
        response["result"]["local"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tab| tab["admitted"].as_bool().unwrap())
            .map(|tab| labels[tab["tab_id"].as_str().unwrap()].clone())
            .collect()
    }

    fn plugin_get(server: &mut HeadlessServer) -> serde_json::Value {
        let (changed, response) = api(
            server,
            api::schema::Method::ScopeGet(api::schema::EmptyParams::default()),
        );
        assert!(!changed, "a scope read never renders");
        response
    }

    fn plugin_admitted(server: &mut HeadlessServer) -> Vec<String> {
        let response = plugin_get(server);
        admitted(server, &response)
    }

    #[tokio::test]
    async fn a_mirror_judges_local_tabs_as_the_client_names_this_machine() {
        let mut server = studio_server();
        let shell = attach(&mut server, 61, false);
        drain_controls(&shell);
        let own = server.app.state.scope.clone();
        assert_eq!(plugin_admitted(&mut server), ["main/fish", "main/outreach"]);

        // By label: the client calls this machine `studio`.
        mirror(
            &mut server,
            &shell,
            61,
            Some(focus(vec![target("studio:main/outreach")])),
        );
        let response = plugin_get(&mut server);
        assert_eq!(admitted(&server, &response), ["main/outreach"]);
        // `scope` stays this server's own; only the verdicts follow the client.
        assert_eq!(
            response["result"]["scope"],
            serde_json::to_value(&own).unwrap()
        );

        // By storage key, and a `local` target is the client's own machine, not
        // this one, so it admits nothing here.
        mirror(
            &mut server,
            &shell,
            61,
            Some(focus(vec![
                ScopeTarget {
                    machine: STUDIO_KEY.into(),
                    workspace_label: Some("admin".into()),
                    ..ScopeTarget::default()
                },
                target("local:main"),
            ])),
        );
        assert_eq!(plugin_admitted(&mut server), ["admin/shell"]);
        // A mirror is not a scope change: nothing projects.
        server.render_and_stream();
        assert!(scope_projections(&drain_controls(&shell)).is_empty());
    }

    #[tokio::test]
    async fn the_foreground_clients_mirror_wins_else_the_most_recently_active() {
        let mut server = studio_server();
        let laptop = attach(&mut server, 62, true);
        let desktop = attach(&mut server, 63, true);
        assert_eq!(server.foreground_client_id, Some(63));
        drain_controls(&laptop);
        drain_controls(&desktop);
        mirror(
            &mut server,
            &laptop,
            62,
            Some(focus(vec![target("studio:main/outreach")])),
        );
        mirror(
            &mut server,
            &desktop,
            63,
            Some(focus(vec![target("studio:admin")])),
        );
        // Mirroring never takes the foreground.
        assert_eq!(server.foreground_client_id, Some(63));
        assert_eq!(plugin_admitted(&mut server), ["admin/shell"]);
        // A client's own read follows its own mirror.
        let own_read = lane(
            &mut server,
            &laptop,
            62,
            api::schema::Method::ScopeGet(api::schema::EmptyParams::default()),
        );
        assert_eq!(admitted(&server, &own_read), ["main/outreach"]);

        // No foreground client: the one active most recently acts.
        server.foreground_client_id = None;
        server.clients.get_mut(&62).unwrap().last_activity = u64::MAX;
        assert_eq!(plugin_admitted(&mut server), ["main/outreach"]);
    }

    #[tokio::test]
    async fn a_disconnect_or_null_drops_the_mirror() {
        let mut server = studio_server();
        let shell = attach(&mut server, 64, false);
        drain_controls(&shell);
        mirror(
            &mut server,
            &shell,
            64,
            Some(focus(vec![target("studio:admin")])),
        );
        assert_eq!(plugin_admitted(&mut server), ["admin/shell"]);
        mirror(&mut server, &shell, 64, None);
        assert!(server.clients[&64].scope_mirror.is_none());
        assert_eq!(plugin_admitted(&mut server), ["main/fish", "main/outreach"]);

        mirror(
            &mut server,
            &shell,
            64,
            Some(focus(vec![target("studio:admin")])),
        );
        server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 64 });
        assert!(!server.clients.contains_key(&64));
        // Back to this server's own scope, which hides `admin`.
        assert_eq!(plugin_admitted(&mut server), ["main/fish", "main/outreach"]);
    }

    #[tokio::test]
    async fn the_socket_api_refuses_a_mirror() {
        let mut server = studio_server();
        let _shell = attach(&mut server, 65, false);
        let (changed, response) = api(
            &mut server,
            api::schema::Method::ScopeMirror(ScopeMirrorParams {
                scope: Some(ScopeState::default()),
                machine: STUDIO_KEY.into(),
                machine_label: "studio".into(),
            }),
        );
        assert!(!changed);
        assert_eq!(response["error"]["code"], "connection_local_only");
        assert!(server.clients[&65].scope_mirror.is_none());
    }
}

#[tokio::test]
async fn pane_flags_projection_connect_change_and_api_optional_field() {
    use api::schema::{EmptyParams, Method, PaneFlagParams, PaneListParams, PaneTarget};
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("flags")];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    let pane = server.app.state.workspaces[0].focused_pane_id().unwrap();
    let id = server.app.public_pane_id(0, pane).unwrap();
    let (_, before) = api(
        &mut server,
        Method::PaneGet(PaneTarget {
            pane_id: id.clone(),
        }),
    );
    assert!(before["result"]["pane"].get("flag").is_none());
    let (changed, response) = api(
        &mut server,
        Method::PaneFlag(PaneFlagParams {
            pane_id: id.clone(),
            message: Some(" ready\n ".into()),
        }),
    );
    assert!(changed);
    assert_eq!(response["result"]["pane"]["flag"]["message"], "ready");
    let first = response["result"]["pane"]["flag"]["seq"].as_u64().unwrap();
    let active = attach(&mut server, 901, true);
    let inactive = attach(&mut server, 902, false);
    let projected = |shell: &TestShell| -> Vec<protocol::endpoint::EndpointPaneFlags> {
        drain_controls(shell)
            .into_iter()
            .filter(|(kind, _)| kind == protocol::endpoint::PANE_FLAGS_KIND)
            .map(|(_, data)| serde_json::from_str(&data).unwrap())
            .collect()
    };
    for shell in [&active, &inactive] {
        let flags = projected(shell);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].flags[0].pane_id, id);
        assert_eq!(flags[0].flags[0].flag.seq, first);
    }
    let (_, replaced) = api(
        &mut server,
        Method::PaneFlag(PaneFlagParams {
            pane_id: id.clone(),
            message: None,
        }),
    );
    let second = replaced["result"]["pane"]["flag"]["seq"].as_u64().unwrap();
    assert!(second > first);
    server.render_and_stream();
    for shell in [&active, &inactive] {
        let flags = projected(shell);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].flags[0].flag.seq, second);
    }
    for method in [
        Method::PaneList(PaneListParams::default()),
        Method::SessionSnapshot(EmptyParams {}),
    ] {
        let (_, response) = api(&mut server, method);
        let panes = response["result"]["panes"]
            .as_array()
            .or_else(|| response["result"]["snapshot"]["panes"].as_array())
            .unwrap();
        assert_eq!(panes[0]["flag"]["seq"], second);
    }
    let (_, stale_clear) = api(
        &mut server,
        Method::PaneUnflag(api::schema::PaneUnflagParams {
            pane_id: id.clone(),
            expected_seq: Some(first),
        }),
    );
    assert_eq!(stale_clear["result"]["changed"], false);
    let (_, current) = api(
        &mut server,
        Method::PaneGet(PaneTarget {
            pane_id: id.clone(),
        }),
    );
    assert_eq!(current["result"]["pane"]["flag"]["seq"], second);
    let (_, matched_clear) = api(
        &mut server,
        Method::PaneUnflag(api::schema::PaneUnflagParams {
            pane_id: id.clone(),
            expected_seq: Some(second),
        }),
    );
    assert_eq!(matched_clear["result"]["changed"], true);
    // Put a flag back to exercise manual, unconditional idempotency too.
    api(
        &mut server,
        Method::PaneFlag(PaneFlagParams {
            pane_id: id.clone(),
            message: None,
        }),
    );
    for expected in [true, false] {
        let (_, response) = api(
            &mut server,
            Method::PaneUnflag(api::schema::PaneUnflagParams {
                pane_id: id.clone(),
                expected_seq: None,
            }),
        );
        assert_eq!(response["result"]["changed"], expected);
    }
    server.render_and_stream();
    for shell in [&active, &inactive] {
        assert!(projected(shell)[0].flags.is_empty());
    }
    let (_, response) = api(&mut server, Method::PaneGet(PaneTarget { pane_id: id }));
    assert!(response["result"]["pane"].get("flag").is_none());
    server.render_and_stream();
    for shell in [&active, &inactive] {
        assert!(projected(shell).is_empty());
    }
}
