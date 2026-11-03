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
        kinds[..3],
        [
            protocol::endpoint::SCOPE_PROJECTION_KIND,
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
