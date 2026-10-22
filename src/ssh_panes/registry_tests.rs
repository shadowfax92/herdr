use super::*;

fn input(terminal: &TerminalId, pane: PaneId, id: &str, host: &str) -> AttachInput {
    AttachInput {
        params: api::AttachParams {
            pane_id: "w1:p1".into(),
            link_id: id.into(),
            host: host.into(),
            connector_pid: 42,
            connector_pgid: 42,
            cwd_hint: Some("/hint".into()),
            protocol: 1,
        },
        pane_id: pane,
        terminal_id: terminal.clone(),
        local_cwd: Some("/local".into()),
    }
}
fn enabled() -> Registry {
    Registry::new(&config::SshPanesConfig {
        enabled: true,
        ..Default::default()
    })
}
fn report(id: &str, seq: u64) -> api::ReportParams {
    api::ReportParams {
        link_id: id.into(),
        seq,
        shell_pid: 80,
        shell_name: "fish".into(),
        foreground: api::RemoteForeground {
            process_group_id: Some(90),
            foreground_is_pane_shell: false,
            agent: Some("claude".into()),
            process_name: Some("claude".into()),
        },
        cwd: Some("/remote".into()),
        foreground_cwd: Some("/remote/job".into()),
    }
}

#[test]
fn ssh_panes_registry_leases_and_takeover_use_injected_instants() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let mut registry = enabled();
    let now = Instant::now();
    let terminal = TerminalId::alloc();
    let pane = PaneId::alloc();
    registry
        .attach(input(&terminal, pane, "one", "studio"), now)
        .unwrap();
    assert_eq!(
        registry.link_for_terminal(&terminal, now).unwrap().state,
        LinkState::Live
    );
    assert_eq!(
        registry
            .link_for_terminal(&terminal, now + SOFT_LEASE)
            .unwrap()
            .state,
        LinkState::Stale
    );
    assert_eq!(
        registry
            .attach(input(&terminal, pane, "two", "studio"), now + SOFT_LEASE)
            .err(),
        Some(SshPanesError::PaneLinked)
    );
    assert!(registry
        .link_for_terminal(&terminal, now + HARD_LEASE)
        .is_none());
    assert_eq!(
        registry.report(&report("one", 1), now + HARD_LEASE).err(),
        Some(SshPanesError::UnknownLink)
    );
    registry
        .attach(input(&terminal, pane, "two", "studio"), now + HARD_LEASE)
        .unwrap();
    assert_eq!(registry.links(now + HARD_LEASE).len(), 1);
    assert_eq!(registry.links(now + HARD_LEASE)[0].link.link_id, "two");
}

#[test]
fn ssh_panes_replayed_reports_neither_mutate_nor_renew_lease() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let mut registry = enabled();
    let now = Instant::now();
    let terminal = TerminalId::alloc();
    registry
        .attach(input(&terminal, PaneId::alloc(), "one", "studio"), now)
        .unwrap();
    // Sequence zero is a valid first report; duplicate zero is not newer.
    assert!(registry.report(&report("one", 0), now).unwrap().accepted);
    let mut replay = report("one", 0);
    replay.cwd = Some("/wrong".into());
    assert!(!registry.report(&replay, now + SOFT_LEASE).unwrap().accepted);
    let view = registry
        .link_for_terminal(&terminal, now + SOFT_LEASE)
        .unwrap();
    assert_eq!(view.state, LinkState::Stale);
    assert_eq!(view.link.cwd.as_deref(), Some("/remote"));
    let next = registry
        .report(&report("one", 1), now + SOFT_LEASE)
        .unwrap();
    assert!(next.accepted && next.changed); // stale -> live is semantic
    let next = registry
        .report(&report("one", 2), now + SOFT_LEASE)
        .unwrap();
    assert!(next.accepted && !next.changed);
}

#[test]
fn ssh_panes_attach_enforces_host_identity_protocol_and_disabled_config() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let now = Instant::now();
    let terminal = TerminalId::alloc();
    let pane = PaneId::alloc();
    let mut registry = Registry::new(&config::SshPanesConfig::default());
    assert_eq!(
        registry
            .attach(input(&terminal, pane, "one", "studio"), now)
            .err(),
        Some(SshPanesError::Disabled)
    );
    registry.set_config(&config::SshPanesConfig {
        enabled: true,
        allowed_hosts: vec!["studio".into()],
        ..Default::default()
    });
    assert_eq!(
        registry
            .attach(input(&terminal, pane, "one", "other"), now)
            .err(),
        Some(SshPanesError::HostNotAllowed)
    );
    let mut invalid = input(&terminal, pane, "one", "studio");
    invalid.params.protocol = 2;
    assert_eq!(
        registry.attach(invalid, now).err(),
        Some(SshPanesError::InvalidRequest)
    );
    registry
        .attach(input(&terminal, pane, "one", "studio"), now)
        .unwrap();
    assert_eq!(
        registry
            .attach(
                input(&TerminalId::alloc(), PaneId::alloc(), "one", "studio"),
                now
            )
            .err(),
        Some(SshPanesError::InvalidRequest)
    );
    assert!(registry.inherit_new_tab());
    registry.report(&report("one", 1), now).unwrap();
    assert!(
        registry
            .set_config(&config::SshPanesConfig::default())
            .disabled_now
    );
    assert_eq!(
        registry.report(&report("one", 2), now).err(),
        Some(SshPanesError::Disabled)
    );
    assert!(registry.links(now).is_empty());
    assert_eq!(registry.linked_terminals().count(), 0);
    assert!(!registry.inherit_new_tab());
    assert!(ssh_probe::remote_agent(pane).is_none());
    registry.set_config(&config::SshPanesConfig {
        enabled: true,
        ..Default::default()
    });
    assert!(registry.links(now).is_empty());
}

#[test]
fn ssh_panes_heartbeat_renews_without_erasing_report_or_its_age() {
    let _table = crate::pane::ssh_probe::TEST_TABLE.blocking_lock();
    let mut registry = enabled();
    let now = Instant::now();
    let terminal = TerminalId::alloc();
    let pane = PaneId::alloc();
    registry
        .attach(input(&terminal, pane, "one", "studio"), now)
        .unwrap();
    registry.report(&report("one", 17), now).unwrap();
    let link = registry
        .attach(input(&terminal, pane, "one", "studio"), now + SOFT_LEASE)
        .unwrap();
    assert_eq!(link.last_seq, 17);
    assert_eq!(link.last_report_at, Some(now));
    assert_eq!(link.state(now + SOFT_LEASE), LinkState::Live);
    assert_eq!(link.cwd.as_deref(), Some("/remote"));
}
