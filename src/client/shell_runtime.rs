use super::*;

pub(super) fn dispatch_client_shell_actions(
    mut actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    mut shell: Option<&mut shell::ClientShellState>,
    detached_process_children: &mut Vec<std::process::Child>,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(Vec<crossterm::event::MouseEvent>, bool), ClientError> {
    let mut replay_mouse = Vec::new();
    let mut repaint = false;
    if let Some(shell) = shell.as_deref_mut() {
        actions.extend(shell.take_scope_requests());
    }
    for action in actions {
        match action {
            shell::ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } => {
                if let Some(connection) = endpoints.connection(&endpoint_id).filter(|connection| {
                    (endpoints.active_id() == &endpoint_id
                        && (endpoints.active_surface_available()
                            // The coherent target is visible before its input fence opens.
                            // Retain workspace navigation; sending remains gated below.
                            || (connection.surface_active
                                && matches!(
                                    request.method,
                                    crate::api::schema::Method::WorkspaceFocus(_)
                                ))))
                        || is_local_scope_request(&endpoint_id, &request.method)
                }) {
                    endpoint_commands.enqueue(endpoint_id, connection.generation, boot_id, request);
                } else if let Some(shell) = shell.as_deref_mut() {
                    repaint |= shell.cancel_endpoint_request(&request.id);
                }
            }
            shell::ClientShellAction::ClipboardWrite(bytes) => {
                crate::selection::write_osc52_bytes(&bytes);
            }
            shell::ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target,
            } => {
                *scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                    endpoint_id,
                    target,
                    force: false,
                });
            }
            shell::ClientShellAction::OpenSafeWebUrl(url) => {
                if crate::app::actions::safe_web_url(&url).is_some() {
                    match crate::platform::open_url(&url) {
                        Ok(Some(child)) => detached_process_children.push(child),
                        Ok(None) => {}
                        Err(err) => warn!(err = %err, url = %url, "failed to open pane URL"),
                    }
                }
            }
            shell::ClientShellAction::ReplayMouse(events) => replay_mouse.extend(events),
            shell::ClientShellAction::Keybind(action) => {
                debug!(
                    ?action,
                    "client shell action awaits its presentation family"
                );
            }
        }
    }
    // A source-off-first handoff leaves the registry's committed identity pointing at a
    // deliberately surface-inactive source. Do not drain its retained queue into a server that
    // must reject it; completion below resumes the committed owner's lane.
    let mut cancelled = Vec::new();
    if endpoints.active_surface_available() {
        let active_endpoint = endpoints.active_id().clone();
        cancelled.extend(endpoint_commands.send_next(&active_endpoint, endpoints));
    }
    // While another machine is active, Local's lane holds only scope requests: an
    // activation retires the source lane, and only `is_local_scope_request` enqueues
    // into an inactive Local. The Local server answers them with its surface off.
    if !endpoints.active_id().is_local() {
        cancelled
            .extend(endpoint_commands.send_next(&endpoint::ClientEndpointId::Local, endpoints));
    }
    if let Some(shell) = shell {
        for request_id in cancelled {
            repaint |= shell.cancel_endpoint_request(&request_id);
        }
    }
    Ok((replay_mouse, repaint))
}

/// Focus and hide live only on the Local server, so its `scope.set` is the one
/// request the shell sends to an endpoint other than the active one (see
/// `ClientShellState::request_scope_set`).
fn is_local_scope_request(
    endpoint_id: &endpoint::ClientEndpointId,
    method: &crate::api::schema::Method,
) -> bool {
    endpoint_id.is_local() && matches!(method, crate::api::schema::Method::ScopeSet(_))
}

/// The scope controls a server may send; see `route_endpoint_scope_control`.
pub(super) enum EndpointScopeControl {
    Projection(crate::scope::ScopeState),
    Pick(crate::scope::ScopePickMode),
}

/// Applies a scope control when it comes from Local and drops it otherwise:
/// focus and hide are the owner's, kept on their local server, and a remote
/// server's scope (or picker request) belongs to whoever runs that server.
/// Returns whether the shell needs a new frame.
pub(super) fn route_endpoint_scope_control(
    shell: &mut shell::ClientShellState,
    endpoint_id: &endpoint::ClientEndpointId,
    control: EndpointScopeControl,
) -> bool {
    if !endpoint_id.is_local() {
        debug!(endpoint = %endpoint_id.storage_key(), "ignoring a remote endpoint's scope control");
        return false;
    }
    match control {
        EndpointScopeControl::Projection(scope) => shell.set_scope_projection(Some(scope)),
        EndpointScopeControl::Pick(mode) => shell.open_scope_picker(mode),
    }
    true
}

/// The client loop's entry for scope controls: routes one, then repaints when the
/// scope or the picker changed what the shell shows. A projection arrives on its
/// own when only the scope changed, so nothing else would trigger that frame.
pub(super) fn route_scope_control(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    control: EndpointScopeControl,
) {
    let Some(shell) = state.shell.as_mut() else {
        return;
    };
    if !route_endpoint_scope_control(shell, endpoint_id, control) {
        return;
    }
    let frame = shell.compose(state.reported_size.0, state.reported_size.1);
    if let Some(frame) = frame {
        state.present_frame(frame);
    }
}

/// Follows the Local endpoint's `scope.projection` capability across (re)connects.
/// Without it, the last projection is stale (an older server cannot update it), so
/// the shell drops it and filters by its own config's hidden workspaces again.
pub(super) fn sync_local_scope_capability(
    shell: &mut shell::ClientShellState,
    endpoint_id: &endpoint::ClientEndpointId,
    supported: bool,
) {
    if endpoint_id.is_local() && !supported {
        shell.set_scope_projection(None);
    }
}

pub(super) fn client_shell_resize_message(
    shell: &shell::ClientShellState,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_mouse: bool,
) -> ClientMessage {
    ClientMessage::ClientShellResize {
        cell_width_px,
        cell_height_px,
        surface_size: shell.surface_size(cols, rows),
        pixel_mouse,
    }
}

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), ClientError> {
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let desired = state.pane_keyboard_report_all || shell.host_keyboard_report_all_requested();
    if desired == state.keyboard_report_all_active {
        return Ok(());
    }
    crate::terminal_modes::set_host_kitty_keyboard_report_all(&mut io::stdout(), desired)
        .map_err(ClientError::ConnectionFailed)?;
    state.keyboard_report_all_active = desired;
    if let Some(shell) = state.shell.as_mut() {
        shell.set_host_reports_all_keys(desired);
    }
    Ok(())
}

pub(super) fn clear_endpoint_host_effects(
    state: &mut ClientState,
    host_mouse_capture_active: &std::sync::atomic::AtomicBool,
    host_sgr_pixels_active: &std::sync::atomic::AtomicBool,
) {
    state.endpoint_mouse_capture_requested = false;
    state.endpoint_sgr_pixels_requested = false;
    let enabled = if state.shell.is_some() {
        state.shell_mouse_capture_preference
    } else {
        state.direct_mouse_capture_preference
    };
    let sgr_pixels =
        super::effective_sgr_pixel_mouse(enabled, false, state.pixel_mouse_available());
    if enabled != state.mouse_capture_active
        || sgr_pixels != host_sgr_pixels_active.load(std::sync::atomic::Ordering::Acquire)
    {
        let _ = super::set_mouse_capture(enabled, sgr_pixels);
    }
    state.mouse_capture_active = enabled;
    host_mouse_capture_active.store(enabled, std::sync::atomic::Ordering::Release);
    host_sgr_pixels_active.store(sgr_pixels, std::sync::atomic::Ordering::Release);

    state.pane_keyboard_report_all = false;
    let _ = sync_client_shell_keyboard_report_all(state);
    let _ = crate::terminal_effects::write_window_title(&mut std::io::stdout(), None);
}

pub(super) fn apply_client_shell_input_source_changes(
    state: &mut ClientState,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) {
    let changes = state
        .shell
        .as_mut()
        .map(shell::ClientShellState::take_input_source_changes)
        .unwrap_or_default();
    for active in changes {
        if active {
            prefix_input_source.switch_to_ascii();
        } else {
            prefix_input_source.restore();
        }
    }
}

fn install_pending_activation(
    state: &mut ClientState,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    activation: endpoint::PendingEndpointActivation,
) {
    // A fresh target epoch must not replay navigation retained by an abandoned handoff.
    let mut retired = endpoint_commands.retire_lane(activation.target());
    if let Some(source) = activation.source_command_lane() {
        retired.extend(endpoint_commands.retire_lane(source));
    }
    if let Some(shell) = state.shell.as_mut() {
        for request_id in retired {
            shell.cancel_endpoint_request(&request_id);
        }
    }
    *next_surface_serial = next_surface_serial.saturating_add(1);
    state.freeze_presentation();
    *pending = Some(activation);
}

fn local_activation_metadata_ready(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> bool {
    endpoints
        .connection(&endpoint::ClientEndpointId::Local)
        .is_some_and(|connection| {
            state.shell.as_ref().is_some_and(|shell| {
                shell
                    .endpoint_snapshot_identity(
                        &endpoint::ClientEndpointId::Local,
                        connection.generation,
                    )
                    .is_some()
            })
        })
}

pub(super) fn take_ready_local_activation(
    state: &mut ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> Option<ClientLoopEvent> {
    if !local_activation_metadata_ready(state, endpoints) {
        return None;
    }
    state
        .deferred_local_activation
        .take()
        .map(|intent| ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: false,
        })
}

pub(super) fn begin_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    endpoint_id: endpoint::ClientEndpointId,
    target: Option<shell::ClientEndpointFocusTarget>,
    force: bool,
    now: std::time::Instant,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(), ClientError> {
    #[cfg(windows)]
    if target.as_ref().is_some_and(|target| {
        !state
            .shell
            .as_ref()
            .is_some_and(|shell| shell.notification_target_is_current(&endpoint_id, target))
    }) {
        return Ok(());
    }
    state.deferred_local_activation = None;
    if endpoint_id.is_local() && !local_activation_metadata_ready(state, endpoints) {
        state.deferred_local_activation = Some(endpoint::EndpointActivationIntent {
            endpoint_id,
            target,
        });
        if let Some(shell) = state.shell.as_mut() {
            shell.receive_endpoint_unavailable(
                "Local is reconnecting; selection will resume when it is ready".into(),
            );
        }
        return Ok(());
    }
    let replace_pending = endpoint_id.is_local()
        && pending
            .as_ref()
            .is_some_and(|activation| !activation.can_retarget(&endpoint_id));
    if !replace_pending {
        if let Some(activation) = pending.as_mut() {
            if activation.can_retarget(&endpoint_id) {
                let retarget_error = activation.retarget(target, endpoints).err();
                if let Some(error) = retarget_error {
                    rollback_endpoint_activation(state, endpoints, pending, error, false);
                }
            } else {
                // Once rollback starts, even a request for the original target is a new intent.
                // Retain it until restoration finishes; Local can instead abandon this handoff.
                let outcome = activation.supersede(endpoint_id, target, endpoints);
                if let endpoint::ActivationRollback::Unavailable(message) = outcome {
                    *pending = None;
                    present_handoff_unavailable(state, message);
                }
            }
            return Ok(());
        }
    }
    let already_active = !replace_pending
        && !force
        && endpoints.active_id() == &endpoint_id
        && endpoints
            .connection(&endpoint_id)
            .is_some_and(|connection| connection.surface_active);
    if already_active {
        if let (Some(shell), Some(target)) = (state.shell.as_mut(), target) {
            let actions = shell.focus_endpoint_target(target);
            let (_, repaint) = dispatch_client_shell_actions(
                actions,
                endpoint_commands,
                endpoints,
                Some(shell),
                &mut state.detached_process_children,
                scheduled_activation,
            )?;
            if repaint {
                if let Some(frame) = shell.compose(state.reported_size.0, state.reported_size.1) {
                    state.present_frame(frame);
                }
            }
        }
        return Ok(());
    }
    let Some(shell) = state.shell.as_ref() else {
        return Ok(());
    };
    let resize = client_shell_resize_message(
        shell,
        state.reported_size.0,
        state.reported_size.1,
        state.reported_cell_size.0,
        state.reported_cell_size.1,
        state.pixel_mouse_available(),
    );
    match endpoint::PendingEndpointActivation::prepare(
        shell,
        endpoints,
        endpoint_id.clone(),
        target,
        resize,
        *next_surface_serial,
        now,
    )
    .and_then(|activation| {
        // Preserve the old transaction if Local fails preflight. After retiring it,
        // all send failures belong to the prepared replacement's rollback path.
        if replace_pending {
            if let Some(previous) = pending.take() {
                previous.abandon(endpoints);
            }
        }
        activation.start(endpoints)
    }) {
        Ok(activation) => install_pending_activation(
            state,
            endpoint_commands,
            pending,
            next_surface_serial,
            activation,
        ),
        Err(endpoint::ActivationBeginError::Preflight(error)) => {
            if let Some(shell) = state.shell.as_mut() {
                shell.receive_endpoint_unavailable(format!(
                    "{}: {error}",
                    shell.endpoint_label(&endpoint_id)
                ));
            }
        }
        Err(endpoint::ActivationBeginError::Partial { activation, error }) => {
            // A send error is not evidence that its peer did not observe the write. Freeze and
            // retain the lifecycle object before rollback so no source or target output can be
            // projected until one ownership path has been proved again.
            install_pending_activation(
                state,
                endpoint_commands,
                pending,
                next_surface_serial,
                *activation,
            );
            rollback_endpoint_activation(
                state,
                endpoints,
                pending,
                format!(
                    "{}: {error}",
                    state
                        .shell
                        .as_ref()
                        .map(|shell| shell.endpoint_label(&endpoint_id).to_owned())
                        .unwrap_or_else(|| format!("{endpoint_id:?}"))
                ),
                false,
            );
        }
    }
    Ok(())
}

pub(super) fn complete_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    let sync_endpoint = pending
        .as_ref()
        .and_then(endpoint::PendingEndpointActivation::presentation_sync_endpoint)
        .cloned();
    if let Some(endpoint_id) = sync_endpoint.as_ref() {
        state.replay_host_theme(endpoints, endpoint_id);
    }
    let completion = {
        let Some(activation) = pending.as_mut() else {
            return Ok(None);
        };
        let Some(shell) = state.shell.as_mut() else {
            return Ok(None);
        };
        match activation.complete(shell, endpoints) {
            Ok(completion) => completion,
            Err(error) => {
                shell.receive_endpoint_unavailable(error);
                return Ok(None);
            }
        }
    };

    if matches!(
        completion,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
    ) {
        // The coherent target frame can replace the frozen source now, but the registry keeps
        // pane input disabled until a second projection epoch has replayed host modes/effects.
        state.unfreeze_presentation();
        let (cleanup, frame) = {
            let shell = state.shell.as_mut().expect("checked client shell");
            (
                shell.take_pending_graphics_cleanup(),
                shell.compose(state.reported_size.0, state.reported_size.1),
            )
        };
        state.present_graphics(&cleanup);
        if let Some(frame) = frame {
            state.present_frame(frame);
        }
        return Ok(None);
    }
    if completion == endpoint::ActivationCompletion::AwaitingPresentationEffects {
        return Ok(None);
    }

    let _ = pending.take();
    endpoints.unfreeze_input();
    let successor = match completion {
        endpoint::ActivationCompletion::RestoredSource {
            error,
            successor: next,
            ..
        } => {
            if next.is_none() {
                if let Some(shell) = state.shell.as_mut() {
                    shell.receive_endpoint_unavailable(error);
                }
            }
            next
        }
        endpoint::ActivationCompletion::Activated => None,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
        | endpoint::ActivationCompletion::AwaitingPresentationEffects => unreachable!(),
    };
    state.unfreeze_presentation();
    if successor.is_none() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    let (cleanup, frame) = {
        let shell = state.shell.as_mut().expect("checked client shell");
        (
            shell.take_pending_graphics_cleanup(),
            shell.compose(state.reported_size.0, state.reported_size.1),
        )
    };
    state.present_graphics(&cleanup);
    if let Some(frame) = frame {
        state.present_frame(frame);
    }
    // A Local selection made while reconnecting is newer than this transaction's successor.
    if let Some(intent) = successor.filter(|_| state.deferred_local_activation.is_none()) {
        return Ok(Some(ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: true,
        }));
    }
    Ok(None)
}

pub(super) fn present_handoff_unavailable(state: &mut ClientState, message: String) {
    // An unavailable committed endpoint has no presentation lease. Keep all pane input and late
    // source output blocked, while allowing this client-owned chrome frame through the freeze.
    state.freeze_presentation();
    let frame = state.shell.as_mut().and_then(|shell| {
        shell.receive_endpoint_unavailable(message);
        shell.compose(state.reported_size.0, state.reported_size.1)
    });
    if let Some(frame) = frame {
        state.present_frozen_chrome(frame);
    }
}

pub(super) fn rollback_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    error: String,
    source_release_rejected: bool,
) {
    let Some(activation) = pending.as_mut() else {
        return;
    };
    match activation.rollback(endpoints, error.clone(), source_release_rejected) {
        endpoint::ActivationRollback::Pending => state.freeze_presentation(),
        endpoint::ActivationRollback::Unavailable(message) => {
            *pending = None;
            // No endpoint has been proven safe to present. Keep pane input frozen, but render
            // the client-owned unavailable chrome rather than silently swallowing the error.
            present_handoff_unavailable(state, message);
        }
    }
}

pub(super) fn handle_endpoint_disconnect(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    notice: &str,
) -> bool {
    supervisors.disconnected(endpoint_id, generation, now);
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id, generation);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                format!("endpoint connection was lost while activating {notice}"),
            );
        match outcome {
            endpoint::ActivationRollback::Pending => {}
            endpoint::ActivationRollback::Unavailable(error) => {
                *pending_activation = None;
                present_handoff_unavailable(state, error);
            }
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.mark_endpoint_disconnected(endpoint_id);
        endpoint_was_active.then(|| format!("{} {notice}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn handle_endpoint_attention(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    message: String,
) -> bool {
    endpoints.disconnect(endpoint_id);
    supervisors.record_status(
        endpoint_id,
        generation,
        endpoint::ClientEndpointStatus::Attention,
        now,
    );
    #[cfg(unix)]
    state.retire_endpoint_graphics(endpoint_id, generation);
    if pending_activation
        .as_ref()
        .is_some_and(|pending| pending.involves_endpoint(endpoint_id))
    {
        let outcome = pending_activation
            .as_mut()
            .expect("checked pending activation")
            .endpoint_disconnected(
                endpoints,
                endpoint_id,
                "endpoint reported attention while activating".into(),
            );
        if let endpoint::ActivationRollback::Unavailable(error) = outcome {
            *pending_activation = None;
            present_handoff_unavailable(state, error);
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.shell.as_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Attention);
        shell.set_machine_diagnostic(endpoint_id, message.clone());
        endpoint_was_active.then(|| format!("{}: {message}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    {
        state.present_frame(frame);
    }
    endpoint_was_active
}

pub(super) fn install_client_shell_snapshot(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    snapshot: Box<crate::protocol::ClientShellSnapshot>,
    projection_pending: bool,
    endpoints: &mut endpoint::EndpointRegistry,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
) -> Result<(), ClientError> {
    let Some(connection) = endpoints.connection(endpoint_id) else {
        return Ok(());
    };
    let generation = connection.generation;
    let project_snapshot =
        !projection_pending && endpoints.active_id() == endpoint_id && connection.surface_active;
    let pixel_mouse_available = state.pixel_mouse_available();
    let (composed, resize, graphics_cleanup) = if let Some(shell) = &mut state.shell {
        let waits_for_selected_surface = projection_pending
            || (endpoints.active_id() == endpoint_id
                && !project_snapshot
                && shell.has_presented_surface());
        let previous_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        if !waits_for_selected_surface {
            shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
        }
        if project_snapshot {
            shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        } else {
            shell.cache_endpoint_snapshot_inactive_for_generation(
                endpoint_id,
                generation,
                snapshot,
            );
        }
        let graphics_cleanup = shell.take_pending_graphics_cleanup();
        let next_size = shell.surface_size(state.reported_size.0, state.reported_size.1);
        (
            shell.compose(state.reported_size.0, state.reported_size.1),
            (previous_size != next_size).then(|| {
                client_shell_resize_message(
                    shell,
                    state.reported_size.0,
                    state.reported_size.1,
                    state.reported_cell_size.0,
                    state.reported_cell_size.1,
                    pixel_mouse_available,
                )
            }),
            graphics_cleanup,
        )
    } else {
        (None, None, Vec::new())
    };
    apply_client_shell_input_source_changes(state, prefix_input_source);
    state.present_graphics(&graphics_cleanup);
    if let Some(resize) = resize {
        endpoints.send_to(endpoint_id, &resize);
    }
    if let Some(frame) = composed {
        if projection_pending {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    }
    Ok(())
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<super::frame_output::ComposedFrame>,
    endpoints: &mut endpoint::EndpointRegistry,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<bool, ClientError> {
    apply_client_shell_input_source_changes(state, prefix_input_source);
    if outcome.detach {
        let _ = write_to_server(endpoints, &ClientMessage::Detach);
        return Ok(true);
    }
    if outcome.resize {
        let shell = state.shell.as_ref().expect("shell mode remains active");
        let resize = client_shell_resize_message(
            shell,
            state.reported_size.0,
            state.reported_size.1,
            state.reported_cell_size.0,
            state.reported_cell_size.1,
            state.pixel_mouse_available(),
        );
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize(resize, endpoints) {
                rollback_endpoint_activation(state, endpoints, pending_activation, error, false);
            }
        } else {
            let _ = write_to_server(endpoints, &resize);
        }
    }
    #[cfg(not(windows))]
    if outcome.query_host_appearance {
        query_host_terminal_appearance();
    }
    if outcome.query_host_theme {
        query_host_terminal_theme();
    }
    sync_client_shell_keyboard_report_all(state)?;
    let (replay, dispatch_repaint) = dispatch_client_shell_actions(
        outcome.actions,
        endpoint_commands,
        endpoints,
        state.shell.as_mut(),
        &mut state.detached_process_children,
        scheduled_activation,
    )?;
    let frame = if dispatch_repaint {
        state
            .shell
            .as_mut()
            .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    } else {
        frame
    };
    debug_assert!(
        replay.is_empty(),
        "mouse replay only follows endpoint results"
    );
    let active_endpoint_online = state
        .shell
        .as_ref()
        .is_none_or(|shell| shell.endpoint_is_online(endpoints.active_id()))
        && endpoints.active_surface_available();
    for request in outcome.requests {
        if let ClientMessage::ClientShellHostTheme { update } = &request {
            state.record_host_theme_update(update);
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_theme(update.clone(), endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        error,
                        false,
                    );
                }
                continue;
            }
            if endpoints.active_surface_available() {
                write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        // Host focus belongs to a pending target even when the source has gone offline or has
        // already had its surface revoked. Route it before the ordinary source-online gate.
        if let ClientMessage::ClientShellFocus { focused } = request {
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_focus(focused, endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        error,
                        false,
                    );
                }
                continue;
            }
            if active_endpoint_online {
                write_to_server(endpoints, &ClientMessage::ClientShellFocus { focused })
                    .map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        if !active_endpoint_online {
            continue;
        }
        if pending_activation.is_some() {
            // Pane input and non-focus host effects do not cross the frozen handoff boundary.
            continue;
        }
        write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
    }
    if let Some(frame) = frame {
        if pending_activation.is_some() {
            state.present_frame(frame);
        } else {
            state.present_frozen_chrome(frame);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_color_reaches_server_before_first_snapshot() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);

        impl endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                self.0.lock().unwrap().push(message.clone());
                Ok(())
            }
        }

        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut endpoints = endpoint::EndpointRegistry::new(
            Capture(sent.clone()),
            1,
            endpoint::EndpointNegotiation::new(Vec::new(), Vec::new()),
        );
        let mut state = ClientState::test_new();
        assert!(!state
            .shell
            .as_ref()
            .unwrap()
            .endpoint_is_online(&endpoint::ClientEndpointId::Local));
        let outcome = state.shell.as_mut().unwrap().handle_raw_events(vec![
            crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Background,
                color: crate::terminal_theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                },
            },
        ]);
        let mut pending_activation = None;
        let mut endpoint_commands = endpoint_commands::EndpointCommands::default();
        let mut prefix_input_source = crate::platform::RealPrefixInputSource::default();
        let mut scheduled_activation = None;
        finish_client_shell_input(
            &mut state,
            outcome,
            None,
            &mut endpoints,
            &mut pending_activation,
            &mut endpoint_commands,
            &mut prefix_input_source,
            &mut scheduled_activation,
        )
        .unwrap();

        assert_eq!(
            *sent.lock().unwrap(),
            vec![ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::DefaultColor {
                    kind: crate::protocol::ClientHostDefaultColorKind::Background,
                    color: crate::protocol::ClientHostColor {
                        r: 0x11,
                        g: 0x22,
                        b: 0x33,
                    },
                },
            }]
        );

        let local = endpoint::ClientEndpointId::Local;
        assert!(endpoints.set_surface_active(&local, false));
        let outcome = state.shell.as_mut().unwrap().handle_raw_events(vec![
            crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Foreground,
                color: crate::terminal_theme::RgbColor { r: 4, g: 5, b: 6 },
            },
        ]);
        finish_client_shell_input(
            &mut state,
            outcome,
            None,
            &mut endpoints,
            &mut pending_activation,
            &mut endpoint_commands,
            &mut prefix_input_source,
            &mut scheduled_activation,
        )
        .unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);

        assert!(endpoints.set_surface_active(&local, true));
        state.replay_host_theme(&mut endpoints, &local);
        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1], messages[0]);
        assert_eq!(
            messages[2],
            ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::DefaultColor {
                    kind: crate::protocol::ClientHostDefaultColorKind::Foreground,
                    color: crate::protocol::ClientHostColor { r: 4, g: 5, b: 6 },
                },
            }
        );
    }

    mod scope {
        //! Focus and hide reach the shell only from Local, and the shell's
        //! `scope.set` reaches Local even while another machine is active.
        use super::*;
        use crate::scope::{ScopeFocus, ScopeSetParams, ScopeState, ScopeTarget};

        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);

        impl endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                self.0.lock().unwrap().push(message.clone());
                Ok(())
            }
        }

        fn remote() -> endpoint::ClientEndpointId {
            endpoint::ClientEndpointId::Ssh(
                endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            )
        }

        fn focused(label: &str) -> ScopeState {
            ScopeState {
                focus: ScopeFocus {
                    active: true,
                    targets: vec![ScopeTarget::parse(label).unwrap()],
                },
                hidden: Vec::new(),
            }
        }

        #[test]
        fn a_local_projection_reaches_effective_scope_and_a_remote_one_does_not() {
            let mut state = ClientState::test_new();
            let shell = state.shell.as_mut().unwrap();
            let fallback = shell.effective_scope().into_owned();

            assert!(!route_endpoint_scope_control(
                shell,
                &remote(),
                EndpointScopeControl::Projection(focused("studio:main")),
            ));
            assert_eq!(*shell.effective_scope(), fallback);

            let local = focused("setup");
            assert!(route_endpoint_scope_control(
                shell,
                &endpoint::ClientEndpointId::Local,
                EndpointScopeControl::Projection(local.clone()),
            ));
            assert_eq!(*shell.effective_scope(), local);
            assert!(!route_endpoint_scope_control(
                shell,
                &remote(),
                EndpointScopeControl::Projection(ScopeState::default()),
            ));
            assert_eq!(*shell.effective_scope(), local);
            // A remote picker request is dropped too.
            assert!(!route_endpoint_scope_control(
                shell,
                &remote(),
                EndpointScopeControl::Pick(crate::scope::ScopePickMode::Hide),
            ));

            // Only Local losing the capability drops the projection, which
            // hands filtering back to the config's hidden workspaces.
            sync_local_scope_capability(shell, &remote(), false);
            sync_local_scope_capability(shell, &endpoint::ClientEndpointId::Local, true);
            assert_eq!(*shell.effective_scope(), local);
            sync_local_scope_capability(shell, &endpoint::ClientEndpointId::Local, false);
            assert_eq!(*shell.effective_scope(), fallback);
        }

        #[test]
        fn scope_set_rides_locals_lane_while_a_remote_is_active() {
            let negotiation = || {
                endpoint::EndpointNegotiation::new(
                    vec!["scope.set".into(), "workspace.focus".into()],
                    Vec::new(),
                )
            };
            let local_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let remote_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut endpoints =
                endpoint::EndpointRegistry::new(Capture(local_sent.clone()), 1, negotiation());
            endpoints.insert(
                remote(),
                Capture(remote_sent.clone()),
                1,
                negotiation(),
                true,
            );
            assert!(endpoints.set_active(&remote()));
            assert!(endpoints.set_surface_active(&endpoint::ClientEndpointId::Local, false));

            let mut state = ClientState::test_new();
            let shell = state.shell.as_mut().unwrap();
            let snapshot: crate::protocol::ClientShellSnapshot =
                serde_json::from_str(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/endpoint-snapshot-v1.json"
                )))
                .unwrap();
            shell.set_snapshot(Box::new(snapshot));
            shell.set_endpoint_status(
                &endpoint::ClientEndpointId::Local,
                endpoint::ClientEndpointStatus::Online,
            );
            let params = ScopeSetParams {
                focus_active: Some(false),
                ..ScopeSetParams::default()
            };
            shell.request_scope_set(params.clone());

            // Any other Local request stays gated while Local is not active.
            let other = shell::ClientShellAction::Endpoint {
                endpoint_id: endpoint::ClientEndpointId::Local,
                boot_id: "boot-v1".into(),
                request: Box::new(crate::api::schema::Request {
                    id: "other".into(),
                    method: crate::api::schema::Method::WorkspaceFocus(
                        crate::api::schema::WorkspaceTarget {
                            workspace_id: "w1".into(),
                        },
                    ),
                }),
            };
            let mut endpoint_commands = endpoint_commands::EndpointCommands::default();
            dispatch_client_shell_actions(
                vec![other],
                &mut endpoint_commands,
                &mut endpoints,
                state.shell.as_mut(),
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();

            let sent = local_sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "{sent:?}");
            let ClientMessage::ClientShellEndpointRequest { boot_id, request } = &sent[0] else {
                panic!("expected an endpoint request, got {:?}", sent[0]);
            };
            assert_eq!(boot_id, "boot-v1");
            let request: crate::api::schema::Request = serde_json::from_str(request).unwrap();
            assert_eq!(request.method, crate::api::schema::Method::ScopeSet(params));
            assert!(remote_sent.lock().unwrap().is_empty());
            assert!(state
                .shell
                .as_mut()
                .unwrap()
                .take_scope_requests()
                .is_empty());
        }
    }
}
