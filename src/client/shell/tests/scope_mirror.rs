//! `scope.mirror`: the shell hands every online remote server that advertises
//! the method the scope it filters by, once per change, connection or label.
//!
//! ```text
//!  Local (no mirror)   studio (advertises scope.mirror)   build (does not)
//! ```
use super::*;
use crate::api::schema::Method;
use crate::client::endpoint::{ClientEndpointStatus, ProfileId, SavedSshEndpoint};
use crate::scope::{ScopeFocus, ScopeMirrorParams, ScopeState, ScopeTarget};

const STUDIO_PROFILE: &str = "0123456789abcdef0123456789abcdef";
const BUILD_PROFILE: &str = "fedcba9876543210fedcba9876543210";

fn studio() -> ClientEndpointId {
    ClientEndpointId::Ssh(ProfileId::parse(STUDIO_PROFILE).unwrap())
}

fn build() -> ClientEndpointId {
    ClientEndpointId::Ssh(ProfileId::parse(BUILD_PROFILE).unwrap())
}

fn profile(id: &str, label: &str) -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse(id).unwrap(),
        label: label.into(),
        target: format!("dev@{label}.example"),
        session: "agents".into(),
        enabled: true,
    }
}

fn booted(boot_id: &str) -> Box<ClientShellSnapshot> {
    let mut snapshot = snapshot();
    snapshot.boot_id = boot_id.into();
    Box::new(snapshot)
}

fn methods(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|name| name.to_string()).collect())
}

fn focus(label: &str) -> ScopeState {
    ScopeState {
        focus: ScopeFocus {
            active: true,
            targets: vec![ScopeTarget::parse(label).unwrap()],
        },
        hidden: Vec::new(),
    }
}

/// Every machine online with a snapshot; only studio advertises the mirror.
/// Whatever the setup itself queued (the config scope's first mirror) is
/// drained, so each test starts from an empty queue.
fn three_machines() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(
        &crate::config::Config::default(),
    ));
    state.set_endpoint_catalog(&[
        profile(STUDIO_PROFILE, "studio"),
        profile(BUILD_PROFILE, "build"),
    ]);
    state.set_endpoint_methods_for(
        &ClientEndpointId::Local,
        methods(&["scope.mirror", "scope.set"]),
    );
    state.set_endpoint_methods_for(&studio(), methods(&["scope.mirror", "workspace.focus"]));
    state.set_endpoint_methods_for(&build(), methods(&["scope.set", "workspace.focus"]));
    state.set_snapshot(booted("local-boot"));
    for (endpoint_id, boot_id) in [(studio(), "studio-boot"), (build(), "build-boot")] {
        state.cache_endpoint_snapshot_for_generation(&endpoint_id, 1, booted(boot_id));
        state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    }
    state.take_scope_requests();
    state
}

/// `(endpoint, boot id, request id, params)` of every queued mirror.
fn take_mirrors(
    state: &mut ClientShellState,
) -> Vec<(ClientEndpointId, String, String, ScopeMirrorParams)> {
    state
        .take_scope_requests()
        .into_iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } => match request.method {
                Method::ScopeMirror(params) => Some((endpoint_id, boot_id, request.id, params)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn the_setup_mirrors_the_config_scope_to_studio_only() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&{
        let mut config = crate::config::Config::default();
        config.ui.sidebar.agents.hidden_workspaces = vec!["ft".into()];
        config
    }));
    state.set_endpoint_catalog(&[profile(STUDIO_PROFILE, "studio")]);
    state.set_endpoint_methods_for(&studio(), methods(&["scope.mirror"]));
    state.cache_endpoint_snapshot_for_generation(&studio(), 1, booted("studio-boot"));
    // Not online yet: nothing to send to.
    assert!(take_mirrors(&mut state).is_empty());
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Online);
    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1);
    // A Local without a projection leaves the config's hides in force, and
    // those are mirrored too.
    assert_eq!(
        mirrors[0].3.scope,
        Some(ScopeState::from_config_hidden(&["ft".into()]))
    );
}

#[test]
fn a_projection_change_mirrors_to_each_online_remote_that_advertises_it() {
    let mut state = three_machines();
    let scope = focus("studio:main");
    state.set_scope_projection(Some(scope.clone()));

    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1, "{mirrors:?}");
    let (endpoint_id, boot_id, request_id, params) = &mirrors[0];
    assert_eq!(endpoint_id, &studio());
    assert_eq!(boot_id, "studio-boot");
    // Named exactly as the client's own panels name studio.
    assert_eq!(
        params,
        &ScopeMirrorParams {
            scope: Some(scope),
            machine: studio().storage_key(),
            machine_label: "studio".into(),
        }
    );
    assert!(state.owns_scope_request(request_id));
}

#[test]
fn an_unchanged_mirror_is_not_resent() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    assert_eq!(take_mirrors(&mut state).len(), 1);

    state.set_scope_projection(Some(focus("studio:main")));
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Online);
    state.set_endpoint_methods_for(&studio(), methods(&["scope.mirror"]));
    // A later snapshot of the same connection and boot.
    let mut later = booted("studio-boot");
    later.revision += 1;
    state.cache_endpoint_snapshot_for_generation(&studio(), 1, later);
    assert!(take_mirrors(&mut state).is_empty());
}

#[test]
fn a_new_boot_or_connection_is_mirrored_again() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    take_mirrors(&mut state);

    // The server restarted: a new boot id.
    state.cache_endpoint_snapshot_for_generation(&studio(), 1, booted("studio-boot-2"));
    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1);
    assert_eq!(mirrors[0].1, "studio-boot-2");

    // The connection dropped and came back to the same server: a new
    // server-side connection, which has no mirror.
    state.cache_endpoint_snapshot_for_generation(&studio(), 2, booted("studio-boot-2"));
    assert_eq!(take_mirrors(&mut state).len(), 1);

    // Even when the new connection reports the same generation and boot (a
    // profile disabled and enabled again), leaving Online forgets the mirror.
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Reconnecting);
    assert!(take_mirrors(&mut state).is_empty());
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Online);
    assert_eq!(take_mirrors(&mut state).len(), 1);
}

#[test]
fn a_renamed_machine_is_mirrored_with_its_new_label() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    take_mirrors(&mut state);

    state.set_endpoint_catalog(&[
        profile(STUDIO_PROFILE, "mac-studio"),
        profile(BUILD_PROFILE, "build"),
    ]);
    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1);
    assert_eq!(mirrors[0].3.machine_label, "mac-studio");
    assert_eq!(mirrors[0].3.machine, studio().storage_key());
}

#[test]
fn a_newer_mirror_supersedes_one_not_yet_dispatched() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    state.set_scope_projection(Some(focus("studio:ops")));

    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1, "only the latest scope leaves");
    assert_eq!(mirrors[0].3.scope, Some(focus("studio:ops")));
    assert_eq!(
        state.scope_mirror_sent_to(&studio()),
        Some(&mirrors[0].3),
        "the dedupe key is the latest mirror"
    );
}

#[test]
fn a_refused_mirror_is_never_retried_for_that_boot_and_never_shows_a_notice() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    let mirrors = take_mirrors(&mut state);
    let refused = Err(ClientShellEndpointError {
        code: Some("unsupported_method".into()),
        message: "method \"scope.mirror\" is not available on this machine".into(),
    });
    assert!(!state.finish_scope_request(&mirrors[0].2, &refused));
    assert!(!state.owns_scope_request(&mirrors[0].2));
    assert_eq!(state.endpoint_notice_text(), None);

    state.set_scope_projection(Some(focus("studio:ops")));
    assert!(take_mirrors(&mut state).is_empty());

    // A new boot may be a newer build: ask it.
    state.cache_endpoint_snapshot_for_generation(&studio(), 1, booted("studio-boot-2"));
    let mirrors = take_mirrors(&mut state);
    assert_eq!(mirrors.len(), 1);
    assert_eq!(mirrors[0].3.scope, Some(focus("studio:ops")));

    // Any other failure only logs: no notice, no retry loop.
    let timeout = Err(ClientShellEndpointError {
        code: Some("endpoint_timeout".into()),
        message: "this server did not respond to the action".into(),
    });
    assert!(!state.finish_scope_request(&mirrors[0].2, &timeout));
    assert!(!state.cancel_endpoint_request("client-shell:unknown"));
    assert_eq!(state.endpoint_notice_text(), None);
    assert!(take_mirrors(&mut state).is_empty());
}

#[test]
fn a_snapshot_for_an_endpoint_already_online_runs_no_mirror_scan() {
    let mut state = three_machines();
    state.set_scope_projection(Some(focus("studio:main")));
    take_mirrors(&mut state);
    let scans = state.scope_mirror_scans();

    // What the client loop does for every snapshot it installs: mark the
    // endpoint Online, then cache the snapshot (same connection and boot).
    let mut later = booted("studio-boot");
    later.revision += 1;
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot_inactive_for_generation(&studio(), 1, later);
    assert_eq!(
        state.scope_mirror_scans(),
        scans,
        "an unchanged snapshot of an online machine never scans"
    );
    assert!(take_mirrors(&mut state).is_empty());

    // Coming back into Online after a drop still mirrors.
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Reconnecting);
    state.set_endpoint_status(&studio(), ClientEndpointStatus::Online);
    assert_eq!(take_mirrors(&mut state).len(), 1);
}

#[test]
fn a_failed_mirror_is_resent_on_the_next_sync_but_a_superseded_one_is_not() {
    let mut state = three_machines();
    let catalog = [
        profile(STUDIO_PROFILE, "studio"),
        profile(BUILD_PROFILE, "build"),
    ];
    state.set_scope_projection(Some(focus("studio:main")));
    let timed_out = take_mirrors(&mut state).remove(0);
    let failure = |code: &str| {
        Err(ClientShellEndpointError {
            code: Some(code.into()),
            message: "the mirror did not land".into(),
        })
    };
    assert!(!state.finish_scope_request(&timed_out.2, &failure("endpoint_timeout")));
    // No immediate retry; the next natural trigger (here a catalog reload)
    // resends it, since the server may never have stored it.
    assert!(take_mirrors(&mut state).is_empty());
    state.set_endpoint_catalog(&catalog);
    let resent = take_mirrors(&mut state);
    assert_eq!(resent.len(), 1, "{resent:?}");
    assert_eq!(resent[0].3, timed_out.3);
    assert_ne!(resent[0].2, timed_out.2);

    // A mirror the lane cancelled because a newer one replaced it: the newer
    // one is still on its way, so nothing is resent.
    state.set_scope_projection(Some(focus("studio:ops")));
    let superseded = take_mirrors(&mut state).remove(0);
    state.set_scope_projection(Some(focus("studio:main/outreach")));
    let newer = take_mirrors(&mut state).remove(0);
    assert!(!state.cancel_endpoint_request(&superseded.2));
    state.set_endpoint_catalog(&catalog);
    assert!(take_mirrors(&mut state).is_empty());
    assert_eq!(state.scope_mirror_sent_to(&studio()), Some(&newer.3));
    assert_eq!(state.endpoint_notice_text(), None);
}
