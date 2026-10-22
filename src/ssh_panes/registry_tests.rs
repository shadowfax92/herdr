use super::*;

fn input(terminal: &TerminalId, pane: PaneId, id: &str, host: &str) -> AttachInput {
    ssh_probe::test_owner(pane, true, Some(42));
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
    assert_eq!(link.state(now + SOFT_LEASE), LinkState::Stale);
    assert_eq!(link.cwd.as_deref(), Some("/remote"));
}

#[test]
fn ssh_panes_ghost_links_lose_inheritance_and_prune_probe_entries() {
    let _table = ssh_probe::TEST_TABLE.blocking_lock();
    for (alive, foreground) in [(false, Some(42)), (true, Some(43)), (true, None)] {
        let mut registry = enabled();
        let now = Instant::now();
        let terminal = TerminalId::alloc();
        let pane = PaneId::alloc();
        registry
            .attach(input(&terminal, pane, "one", "studio"), now)
            .unwrap();
        registry.report(&report("one", 1), now).unwrap();
        ssh_probe::test_owner(pane, alive, foreground);
        assert!(registry.inherit_target(&terminal, now).is_none());
        assert!(registry.link_for_terminal(&terminal, now).is_none());
        assert!(registry.links(now).is_empty());
        assert_eq!(registry.refresh(now), vec![terminal.clone()]);
        assert_eq!(registry.linked_terminals().count(), 0);
        assert_eq!(ssh_probe::entry_count(), 0);
        registry
            .attach(input(&terminal, pane, "two", "studio"), now)
            .unwrap();
        assert_eq!(registry.links(now)[0].link.link_id, "two");
    }
}

#[test]
fn ssh_panes_watcher_age_is_independent_of_attach_heartbeats() {
    let _table = ssh_probe::TEST_TABLE.blocking_lock();
    let mut registry = enabled();
    let now = Instant::now();
    let terminal = TerminalId::alloc();
    let pane = PaneId::alloc();
    registry
        .attach(input(&terminal, pane, "one", "studio"), now)
        .unwrap();
    registry.report(&report("one", 1), now).unwrap();
    for seconds in [10, 20, 110, 125] {
        let later = now + Duration::from_secs(seconds);
        registry
            .attach(input(&terminal, pane, "one", "studio"), later)
            .unwrap();
        let view = registry.link_for_terminal(&terminal, later).unwrap();
        assert_eq!(
            view.state,
            if seconds < 15 {
                LinkState::Live
            } else {
                LinkState::Stale
            }
        );
        if seconds >= 15 {
            assert_eq!(
                registry
                    .inherit_target(&terminal, later)
                    .unwrap()
                    .cwd
                    .as_deref(),
                Some("/remote")
            );
        }
    }
    let later = now + Duration::from_secs(126);
    assert!(registry.report(&report("one", 2), later).unwrap().changed);
    assert_eq!(
        registry.link_for_terminal(&terminal, later).unwrap().state,
        LinkState::Live
    );
}

#[test]
fn ssh_panes_provisional_links_are_live_for_thirty_seconds_and_adopted_by_their_connector() {
    let _table = ssh_probe::TEST_TABLE.blocking_lock();
    let now = Instant::now();
    for adopt in [false, true] {
        let mut registry = enabled();
        let terminal = TerminalId::alloc();
        let pane = PaneId::alloc();
        registry
            .provision(input(&terminal, pane, "pending", "studio"), now)
            .unwrap();
        let view = registry
            .link_for_terminal(&terminal, now + Duration::from_secs(29))
            .unwrap();
        assert_eq!(view.state, LinkState::Live);
        assert_eq!(view.link.cwd.as_deref(), Some("/hint"));
        if adopt {
            let mut wrong = input(&terminal, pane, "wrong", "studio");
            wrong.params.connector_pid = 43;
            assert_eq!(
                registry.attach(wrong, now).err(),
                Some(SshPanesError::InvalidRequest)
            );
            registry
                .attach(
                    input(&terminal, pane, "real", "studio"),
                    now + Duration::from_secs(29),
                )
                .unwrap();
            assert_eq!(registry.links(now + PROVISIONAL_LEASE).len(), 1);
            assert_eq!(
                registry.links(now + PROVISIONAL_LEASE)[0].link.link_id,
                "real"
            );
        } else {
            assert_eq!(registry.refresh(now + PROVISIONAL_LEASE), vec![terminal]);
            assert!(registry.links(now + PROVISIONAL_LEASE).is_empty());
            assert_eq!(ssh_probe::entry_count(), 0);
        }
    }
}

#[test]
fn ssh_panes_allowed_hosts_live_reload_revokes_only_disallowed_links() {
    let _table = ssh_probe::TEST_TABLE.blocking_lock();
    let mut registry = enabled();
    let now = Instant::now();
    let a = TerminalId::alloc();
    let b = TerminalId::alloc();
    registry
        .attach(input(&a, PaneId::alloc(), "a", "studio"), now)
        .unwrap();
    registry
        .attach(input(&b, PaneId::alloc(), "b", "other"), now)
        .unwrap();
    let change = registry.set_config(&config::SshPanesConfig {
        enabled: true,
        allowed_hosts: vec!["studio".into()],
        ..Default::default()
    });
    assert!(!change.disabled_now);
    assert_eq!(change.affected, vec![b.clone()]);
    assert!(registry.link_for_terminal(&a, now).is_some());
    assert!(registry.inherit_target(&b, now).is_none());
    assert_eq!(
        registry.report(&report("b", 1), now).err(),
        Some(SshPanesError::UnknownLink)
    );
    assert_eq!(ssh_probe::entry_count(), 1);
}

#[cfg(unix)]
#[test]
fn ssh_panes_connector_death_uses_real_platform_liveness() {
    let _table = ssh_probe::TEST_TABLE.blocking_lock();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let exited_pid = child.id();
    child.wait().unwrap();
    let now = Instant::now();
    for (pid, expected) in [(std::process::id(), true), (exited_pid, false)] {
        let mut registry = enabled();
        let pane = PaneId::alloc();
        let terminal = TerminalId::alloc();
        registry
            .attach(
                AttachInput {
                    params: api::AttachParams {
                        pane_id: "w1:p1".into(),
                        link_id: "one".into(),
                        host: "studio".into(),
                        connector_pid: pid,
                        connector_pgid: 42,
                        cwd_hint: None,
                        protocol: 1,
                    },
                    pane_id: pane,
                    terminal_id: terminal.clone(),
                    local_cwd: None,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            registry.link_for_terminal(&terminal, now).is_some(),
            expected
        );
        registry.refresh(now);
        assert_eq!(ssh_probe::entry_count(), usize::from(expected));
    }
}
