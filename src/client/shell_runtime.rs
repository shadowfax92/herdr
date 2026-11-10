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
                    if matches!(request.method, crate::api::schema::Method::PaneUnflag(_))
                        && !connection.negotiation.supports_method("pane.unflag")
                    {
                        return false;
                    }
                    (endpoints.active_id() == &endpoint_id
                        && (endpoints.active_surface_available()
                            // The coherent target is visible before its input fence opens.
                            // Retain workspace navigation; sending remains gated below.
                            || (connection.surface_active
                                && matches!(
                                    request.method,
                                    crate::api::schema::Method::WorkspaceFocus(_)
                                ))))
                        || is_scope_lane_request(&endpoint_id, &request.method)
                }) {
                    let superseded = endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation,
                        boot_id,
                        request,
                    );
                    if let Some(shell) = shell.as_deref_mut() {
                        for request_id in superseded {
                            repaint |= shell.cancel_endpoint_request(&request_id);
                        }
                    }
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
    if endpoints.active_surface_available() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = shell.as_deref_mut() {
            for request_id in cancelled {
                repaint |= shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    repaint |= pump_scope_lanes(endpoint_commands, endpoints, shell);
    Ok((replay_mouse, repaint))
}

/// Settles a completed endpoint command. Local's scope writes settle whichever
/// endpoint is active (their lane is Local's, which may be inactive), so a failure
/// reaches the user instead of being cancelled with the inactive lane. Other
/// results reach the shell only from the active endpoint's current connection
/// and are otherwise cancelled.
pub(super) fn settle_completed_command(
    shell: &mut shell::ClientShellState,
    completed: endpoint_commands::EndpointCommandResult,
    generation: u64,
) -> (bool, Vec<shell::ClientShellAction>) {
    if shell.owns_scope_request(&completed.request_id) {
        return (
            shell.finish_scope_request(&completed.request_id, &completed.result),
            Vec::new(),
        );
    }
    if completed.generation == generation && shell.endpoint_is_active(&completed.endpoint_id) {
        shell.handle_endpoint_result(&completed.boot_id, &completed.request_id, completed.result)
    } else {
        (
            shell.cancel_endpoint_request(&completed.request_id),
            Vec::new(),
        )
    }
}

/// Sends the next scope request on every lane but the active one: Local's scope
/// writes while another endpoint is active, and each remote server's scope
/// mirror while it is not presented. Those lanes hold only scope requests: an
/// activation retires everything else on them, and only `is_scope_lane_request`
/// enqueues into an inactive lane. The servers answer them with their surface
/// off. Returns whether a cancellation needs a frame.
pub(super) fn pump_scope_lanes(
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    mut shell: Option<&mut shell::ClientShellState>,
) -> bool {
    let active = endpoints.active_id().clone();
    let mut repaint = false;
    for endpoint_id in endpoint_commands.inactive_lanes_with_queued(&active) {
        let cancelled = endpoint_commands.send_next(&endpoint_id, endpoints);
        if let Some(shell) = shell.as_deref_mut() {
            for request_id in cancelled {
                repaint |= shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    repaint
}

/// The requests the shell sends to an endpoint other than the active one. Focus
/// and hide live only on the Local server, so its `scope.set` goes there
/// (`ClientShellState::request_scope_set`); and every remote server gets this
/// client's scope as `scope.mirror` (`ClientShellState::sync_scope_mirrors`),
/// so its plugins filter as the panels do.
fn is_scope_lane_request(
    endpoint_id: &endpoint::ClientEndpointId,
    method: &crate::api::schema::Method,
) -> bool {
    match method {
        crate::api::schema::Method::ScopeSet(_) => endpoint_id.is_local(),
        crate::api::schema::Method::ScopeMirror(_) => !endpoint_id.is_local(),
        _ => false,
    }
}

/// Applies one scope control to the shell and returns the input outcome to
/// finish, or `None` when the control is dropped. A projection counts only from
/// Local: focus and hide are the owner's and live on their local server, and a
/// remote server's scope belongs to whoever runs it. Pick and toggle count from
/// any endpoint, because a plugin menu runs on the active machine's server;
/// they only open the picker or flip focus, and every resulting `scope.set`
/// goes to Local (`request_scope_set`).
pub(super) fn route_endpoint_scope_control(
    shell: &mut shell::ClientShellState,
    endpoint_id: &endpoint::ClientEndpointId,
    control: endpoint::ScopeControl,
) -> Option<shell::ClientShellInput> {
    let repaint = || shell::ClientShellInput {
        repaint: true,
        ..shell::ClientShellInput::default()
    };
    match control {
        endpoint::ScopeControl::Projection(projection) => {
            if !endpoint_id.is_local() {
                debug!(
                    endpoint = %endpoint_id.storage_key(),
                    "ignoring a remote endpoint's scope projection"
                );
                return None;
            }
            shell.set_scope_projection(Some(projection.scope));
            Some(repaint())
        }
        endpoint::ScopeControl::Pick(mode) => {
            shell.open_scope_picker(mode);
            Some(repaint())
        }
        endpoint::ScopeControl::Toggle => Some(shell.toggle_scope_focus_from_endpoint()),
    }
}

/// The client loop's entry for scope controls. The outcome goes through the
/// same finish as keyboard input: a toggle's `scope.set` and any jump into the
/// new scope are dispatched, and a projection that arrived alone (only the
/// scope changed) still gets its frame. Returns whether the client should exit.
pub(super) fn finish_scope_control(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    control: endpoint::ScopeControl,
    endpoints: &mut endpoint::EndpointRegistry,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint_commands::EndpointCommands,
    prefix_input_source: &mut impl crate::platform::PrefixInputSource,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<bool, ClientError> {
    let (outcome, frame) = {
        let Some(shell) = state.shell.as_mut() else {
            return Ok(false);
        };
        let Some(outcome) = route_endpoint_scope_control(shell, endpoint_id, control) else {
            return Ok(false);
        };
        let frame = outcome
            .repaint
            .then(|| shell.compose(state.reported_size.0, state.reported_size.1))
            .flatten();
        (outcome, frame)
    };
    finish_client_shell_input(
        state,
        outcome,
        frame,
        endpoints,
        pending_activation,
        endpoint_commands,
        prefix_input_source,
        scheduled_activation,
    )
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
    // The retired source lane kept its scope requests; Local's writes and remote
    // mirrors leave now, even while the newly active machine (or a successor
    // handoff) owns the surface.
    pump_scope_lanes(endpoint_commands, endpoints, state.shell.as_mut());
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
    fn pane_flags_registry_never_sends_an_unadvertised_unflag() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);
        impl endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                self.0.lock().unwrap().push(message.clone());
                Ok(())
            }
        }
        for supported in [false, true] {
            let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let methods = if supported {
                vec!["pane.unflag".into()]
            } else {
                Vec::new()
            };
            let mut endpoints = endpoint::EndpointRegistry::new(
                Capture(sent.clone()),
                1,
                endpoint::EndpointNegotiation::new(methods, Vec::new()),
            );
            let action = shell::ClientShellAction::Endpoint {
                endpoint_id: endpoint::ClientEndpointId::Local,
                boot_id: "test-boot".into(),
                request: Box::new(crate::api::schema::Request {
                    id: "clear-flag".into(),
                    method: crate::api::schema::Method::PaneUnflag(
                        crate::api::schema::PaneUnflagParams {
                            pane_id: "pane_1".into(),
                            expected_seq: Some(1),
                        },
                    ),
                }),
            };
            dispatch_client_shell_actions(
                vec![action],
                &mut endpoint_commands::EndpointCommands::default(),
                &mut endpoints,
                None,
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();
            assert_eq!(!sent.lock().unwrap().is_empty(), supported);
        }
    }

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
        //! The scope projection counts only from Local; pick and toggle count
        //! from any endpoint; and the shell's `scope.set` reaches Local even
        //! while another machine is active.
        use super::*;
        use crate::scope::{ScopeFocus, ScopePickMode, ScopeSetParams, ScopeState, ScopeTarget};

        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);

        impl endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                self.0.lock().unwrap().push(message.clone());
                Ok(())
            }
        }

        type Sent = std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>;

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

        fn projection(scope: ScopeState) -> endpoint::ScopeControl {
            endpoint::ScopeControl::Projection(crate::protocol::endpoint::EndpointScopeProjection {
                boot_id: "boot".into(),
                revision: 1,
                scope,
            })
        }

        /// A shell whose Local endpoint is online, viewing a remote machine:
        /// the registry's active endpoint is the remote and Local's surface is
        /// off. Returns what each endpoint's transport is sent.
        fn viewing_a_remote() -> (ClientState, endpoint::EndpointRegistry, Sent, Sent) {
            let negotiation = || {
                endpoint::EndpointNegotiation::new(
                    vec!["scope.set".into(), "workspace.focus".into()],
                    Vec::new(),
                )
            };
            let local_sent = Sent::default();
            let remote_sent = Sent::default();
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
            (state, endpoints, local_sent, remote_sent)
        }

        fn viewing_a_remote_with_commands() -> (
            ClientState,
            endpoint::EndpointRegistry,
            Sent,
            endpoint_commands::EndpointCommands,
        ) {
            let (state, endpoints, local_sent, _remote_sent) = viewing_a_remote();
            (
                state,
                endpoints,
                local_sent,
                endpoint_commands::EndpointCommands::default(),
            )
        }

        /// The `scope.set` params of every request Local was sent.
        fn scope_sets(sent: &Sent) -> Vec<ScopeSetParams> {
            sent.lock()
                .unwrap()
                .iter()
                .map(|message| {
                    let ClientMessage::ClientShellEndpointRequest { boot_id, request } = message
                    else {
                        panic!("expected an endpoint request, got {message:?}");
                    };
                    assert_eq!(boot_id, "boot-v1");
                    let request: crate::api::schema::Request =
                        serde_json::from_str(request).unwrap();
                    let crate::api::schema::Method::ScopeSet(params) = request.method else {
                        panic!("expected scope.set, got {:?}", request.method);
                    };
                    params
                })
                .collect()
        }

        #[test]
        fn a_local_projection_reaches_effective_scope_and_a_remote_one_does_not() {
            let mut state = ClientState::test_new();
            let shell = state.shell.as_mut().unwrap();
            let fallback = shell.effective_scope().into_owned();

            assert!(route_endpoint_scope_control(
                shell,
                &remote(),
                projection(focused("studio:main"))
            )
            .is_none());
            assert_eq!(*shell.effective_scope(), fallback);

            let local = focused("setup");
            let outcome = route_endpoint_scope_control(
                shell,
                &endpoint::ClientEndpointId::Local,
                projection(local.clone()),
            )
            .expect("Local projection applies");
            assert!(outcome.repaint && outcome.actions.is_empty());
            assert_eq!(*shell.effective_scope(), local);
            assert!(route_endpoint_scope_control(
                shell,
                &remote(),
                projection(ScopeState::default())
            )
            .is_none());
            assert_eq!(*shell.effective_scope(), local);

            // Only Local losing the capability drops the projection, which
            // hands filtering back to the config's hidden workspaces.
            sync_local_scope_capability(shell, &remote(), false);
            sync_local_scope_capability(shell, &endpoint::ClientEndpointId::Local, true);
            assert_eq!(*shell.effective_scope(), local);
            sync_local_scope_capability(shell, &endpoint::ClientEndpointId::Local, false);
            assert_eq!(*shell.effective_scope(), fallback);
        }

        #[test]
        fn a_remote_pick_opens_the_picker() {
            let mut state = ClientState::test_new();
            let shell = state.shell.as_mut().unwrap();
            assert_eq!(shell.scope_picker_mode(), None);
            let outcome = route_endpoint_scope_control(
                shell,
                &remote(),
                endpoint::ScopeControl::Pick(ScopePickMode::Hide),
            )
            .expect("a pick from any endpoint applies");
            assert!(outcome.repaint);
            assert_eq!(shell.scope_picker_mode(), Some(ScopePickMode::Hide));
            // A second pick, from Local, only switches the open picker's mode.
            route_endpoint_scope_control(
                shell,
                &endpoint::ClientEndpointId::Local,
                endpoint::ScopeControl::Pick(ScopePickMode::Focus),
            );
            assert_eq!(shell.scope_picker_mode(), Some(ScopePickMode::Focus));
        }

        #[test]
        fn a_remote_toggle_flips_focus_on_local() {
            let (mut state, mut endpoints, local_sent, remote_sent) = viewing_a_remote();
            let shell = state.shell.as_mut().unwrap();
            route_endpoint_scope_control(
                shell,
                &endpoint::ClientEndpointId::Local,
                projection(focused("setup")),
            );
            let outcome =
                route_endpoint_scope_control(shell, &remote(), endpoint::ScopeControl::Toggle)
                    .expect("a toggle from any endpoint applies");
            assert!(outcome.repaint);

            let mut endpoint_commands = endpoint_commands::EndpointCommands::default();
            dispatch_client_shell_actions(
                outcome.actions,
                &mut endpoint_commands,
                &mut endpoints,
                state.shell.as_mut(),
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();

            // Focus was on, so the toggle asks Local to switch it off. The
            // scope itself changes only when Local projects it back.
            assert_eq!(
                scope_sets(&local_sent),
                [ScopeSetParams {
                    focus_active: Some(false),
                    ..ScopeSetParams::default()
                }]
            );
            assert!(remote_sent.lock().unwrap().is_empty());
            assert!(state.shell.as_ref().unwrap().effective_scope().focus.active);
        }

        #[test]
        fn scope_set_rides_locals_lane_while_a_remote_is_active() {
            let (mut state, mut endpoints, local_sent, remote_sent) = viewing_a_remote();
            let params = ScopeSetParams {
                focus_active: Some(false),
                ..ScopeSetParams::default()
            };
            state
                .shell
                .as_mut()
                .unwrap()
                .request_scope_set(params.clone());

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

            assert_eq!(scope_sets(&local_sent), [params]);
            assert!(remote_sent.lock().unwrap().is_empty());
            assert!(state
                .shell
                .as_mut()
                .unwrap()
                .take_scope_requests()
                .is_empty());
        }

        /// Local is active with a navigation command in flight when focus is
        /// applied; the jump to another machine then retires Local's lane.
        fn local_active_with_a_command_in_flight() -> (
            ClientState,
            endpoint::EndpointRegistry,
            Sent,
            endpoint_commands::EndpointCommands,
        ) {
            let (mut state, mut endpoints, local_sent, _remote_sent) = viewing_a_remote();
            assert!(endpoints.set_surface_active(&endpoint::ClientEndpointId::Local, true));
            assert!(endpoints.set_active(&endpoint::ClientEndpointId::Local));
            let mut endpoint_commands = endpoint_commands::EndpointCommands::default();
            let navigation = shell::ClientShellAction::Endpoint {
                endpoint_id: endpoint::ClientEndpointId::Local,
                boot_id: "boot-v1".into(),
                request: Box::new(crate::api::schema::Request {
                    id: "navigation".into(),
                    method: crate::api::schema::Method::WorkspaceFocus(
                        crate::api::schema::WorkspaceTarget {
                            workspace_id: "w1".into(),
                        },
                    ),
                }),
            };
            dispatch_client_shell_actions(
                vec![navigation],
                &mut endpoint_commands,
                &mut endpoints,
                state.shell.as_mut(),
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();
            assert_eq!(local_sent.lock().unwrap().len(), 1);
            local_sent.lock().unwrap().clear();
            (state, endpoints, local_sent, endpoint_commands)
        }

        /// Retires both lanes and switches to the remote, as
        /// `install_pending_activation` and its completion do.
        fn switch_to_the_remote(
            state: &mut ClientState,
            endpoints: &mut endpoint::EndpointRegistry,
            endpoint_commands: &mut endpoint_commands::EndpointCommands,
        ) {
            let mut retired = endpoint_commands.retire_lane(&remote());
            retired.extend(endpoint_commands.retire_lane(&endpoint::ClientEndpointId::Local));
            for request_id in retired {
                state
                    .shell
                    .as_mut()
                    .unwrap()
                    .cancel_endpoint_request(&request_id);
            }
            assert!(endpoints.set_active(&remote()));
            assert!(endpoints.set_surface_active(&endpoint::ClientEndpointId::Local, false));
        }

        #[test]
        fn a_machine_switch_keeps_a_queued_scope_write_and_sends_it() {
            let (mut state, mut endpoints, local_sent, mut endpoint_commands) =
                local_active_with_a_command_in_flight();
            let params = ScopeSetParams {
                focus_active: Some(true),
                ..ScopeSetParams::default()
            };
            state
                .shell
                .as_mut()
                .unwrap()
                .request_scope_set(params.clone());
            // Queued behind the navigation still in flight.
            dispatch_client_shell_actions(
                Vec::new(),
                &mut endpoint_commands,
                &mut endpoints,
                state.shell.as_mut(),
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();
            assert!(local_sent.lock().unwrap().is_empty());

            switch_to_the_remote(&mut state, &mut endpoints, &mut endpoint_commands);
            pump_scope_lanes(&mut endpoint_commands, &mut endpoints, state.shell.as_mut());

            assert_eq!(scope_sets(&local_sent), [params]);
            assert_eq!(state.shell.as_ref().unwrap().endpoint_notice_text(), None);
        }

        #[test]
        fn a_machine_switch_keeps_a_scope_write_in_flight() {
            let (mut state, mut endpoints, local_sent, mut endpoint_commands) =
                viewing_a_remote_with_commands();
            state
                .shell
                .as_mut()
                .unwrap()
                .request_scope_set(ScopeSetParams {
                    focus_active: Some(false),
                    ..ScopeSetParams::default()
                });
            dispatch_client_shell_actions(
                Vec::new(),
                &mut endpoint_commands,
                &mut endpoints,
                state.shell.as_mut(),
                &mut Vec::new(),
                &mut None,
            )
            .unwrap();
            assert_eq!(scope_sets(&local_sent).len(), 1);
            let ClientMessage::ClientShellEndpointRequest { request, .. } =
                local_sent.lock().unwrap()[0].clone()
            else {
                unreachable!();
            };
            let request_id = serde_json::from_str::<crate::api::schema::Request>(&request)
                .unwrap()
                .id;

            // Retiring Local's lane neither cancels the write nor drops its answer.
            assert!(endpoint_commands
                .retire_lane(&endpoint::ClientEndpointId::Local)
                .is_empty());
            let answer = serde_json::to_vec(&crate::api::schema::SuccessResponse {
                id: request_id.clone(),
                result: crate::api::schema::ResponseResult::Ok {},
            })
            .unwrap();
            let completed = endpoint_commands
                .receive_chunk(
                    &endpoint::ClientEndpointId::Local,
                    1,
                    "boot-v1",
                    &request_id,
                    true,
                    answer,
                )
                .unwrap()
                .expect("the kept write completes");
            assert!(completed.result.is_ok());
        }

        #[test]
        fn a_failed_or_interrupted_scope_write_tells_the_user_and_success_does_not() {
            let (mut state, ..) = viewing_a_remote();
            let shell = state.shell.as_mut().unwrap();
            let mut ids = Vec::new();
            for _ in 0..3 {
                shell.request_scope_set(ScopeSetParams::default());
            }
            for action in shell.take_scope_requests() {
                let shell::ClientShellAction::Endpoint { request, .. } = action else {
                    unreachable!();
                };
                ids.push(request.id);
            }

            let (repaint, actions) = shell.handle_endpoint_result(
                "boot-v1",
                &ids[0],
                Ok(crate::api::schema::ResponseResult::Ok {}),
            );
            assert!(!repaint && actions.is_empty());
            assert_eq!(shell.endpoint_notice_text(), None);

            shell.handle_endpoint_result(
                "boot-v1",
                &ids[1],
                Err(shell::ClientShellEndpointError {
                    code: Some("invalid_params".into()),
                    message: "bad target".into(),
                }),
            );
            let notice = shell.endpoint_notice_text().expect("a failure notice");
            assert!(
                notice.contains("did not change") && notice.contains("bad target"),
                "{notice}"
            );

            assert!(shell.cancel_endpoint_request(&ids[2]));
            let notice = shell
                .endpoint_notice_text()
                .expect("an interruption notice");
            assert!(notice.contains("did not change"), "{notice}");
        }

        #[test]
        fn an_inactive_locals_scope_results_are_settled_not_cancelled() {
            let (mut state, ..) = viewing_a_remote();
            let shell = state.shell.as_mut().unwrap();
            for _ in 0..2 {
                shell.request_scope_set(ScopeSetParams::default());
            }
            let ids: Vec<String> = shell
                .take_scope_requests()
                .into_iter()
                .map(|action| match action {
                    shell::ClientShellAction::Endpoint { request, .. } => request.id,
                    _ => unreachable!(),
                })
                .collect();
            let completed = |request_id: &str, result| endpoint_commands::EndpointCommandResult {
                endpoint_id: endpoint::ClientEndpointId::Local,
                generation: 1,
                boot_id: "boot-v1".into(),
                request_id: request_id.into(),
                result,
            };

            // Success on the inactive Local: settled quietly, never "interrupted".
            let (repaint, _) = settle_completed_command(
                shell,
                completed(&ids[0], Ok(crate::api::schema::ResponseResult::Ok {})),
                1,
            );
            assert!(!repaint);
            assert!(!shell.owns_scope_request(&ids[0]));
            assert_eq!(shell.endpoint_notice_text(), None);

            // A timeout on the inactive Local is reported.
            let (repaint, _) = settle_completed_command(
                shell,
                completed(
                    &ids[1],
                    Err(shell::ClientShellEndpointError {
                        code: Some("endpoint_timeout".into()),
                        message: "this server did not respond to the action".into(),
                    }),
                ),
                1,
            );
            assert!(repaint);
            let notice = shell.endpoint_notice_text().expect("a timeout notice");
            assert!(notice.contains("did not change") && notice.contains("did not respond"));
        }

        #[test]
        fn consecutive_toggles_alternate_until_local_confirms() {
            let (mut state, ..) = viewing_a_remote();
            let shell = state.shell.as_mut().unwrap();
            shell.set_scope_projection(Some(focused("setup")));
            let toggled = |shell: &mut shell::ClientShellState| {
                shell.toggle_scope_focus_from_endpoint();
                let actions = shell.take_scope_requests();
                let [shell::ClientShellAction::Endpoint { request, .. }] = actions.as_slice()
                else {
                    panic!("one scope.set per toggle");
                };
                let crate::api::schema::Method::ScopeSet(params) = &request.method else {
                    unreachable!();
                };
                (request.id.clone(), params.focus_active)
            };
            assert_eq!(toggled(shell).1, Some(false));
            assert_eq!(toggled(shell).1, Some(true));
            assert_eq!(toggled(shell).1, Some(false));

            // Local confirms the last value asked for: toggles follow it again.
            let mut off = focused("setup");
            off.focus.active = false;
            shell.set_scope_projection(Some(off));
            let (failing, active) = toggled(shell);
            assert_eq!(active, Some(true));

            // That write fails, so focus stays off and the next toggle turns it
            // on again rather than flipping the failed value.
            shell.handle_endpoint_result(
                "boot-v1",
                &failing,
                Err(shell::ClientShellEndpointError {
                    code: Some("server_unavailable".into()),
                    message: "server is shutting down".into(),
                }),
            );
            assert_eq!(toggled(shell).1, Some(true));
        }

        mod mirror {
            //! `scope.mirror` rides each remote server's own lane whichever
            //! machine is presented, survives machine switches, and a newer
            //! mirror supersedes one still queued.
            use super::*;
            use crate::scope::ScopeMirrorParams;

            /// Local presented; studio online beside it with its surface off,
            /// advertising `scope.mirror`. The shell has already queued studio's
            /// first mirror (the config scope).
            fn presenting_local_beside_studio() -> (
                ClientState,
                endpoint::EndpointRegistry,
                Sent,
                endpoint_commands::EndpointCommands,
            ) {
                let negotiation = |methods: &[&str]| {
                    endpoint::EndpointNegotiation::new(
                        methods.iter().map(|method| method.to_string()).collect(),
                        Vec::new(),
                    )
                };
                let local_sent = Sent::default();
                let remote_sent = Sent::default();
                let mut endpoints = endpoint::EndpointRegistry::new(
                    Capture(local_sent),
                    1,
                    negotiation(&["scope.set"]),
                );
                endpoints.insert(
                    remote(),
                    Capture(remote_sent.clone()),
                    1,
                    negotiation(&["scope.mirror"]),
                    false,
                );
                assert!(endpoints.active_id().is_local());

                let mut state = ClientState::test_new();
                let shell = state.shell.as_mut().unwrap();
                shell.set_endpoint_catalog(&[endpoint::SavedSshEndpoint {
                    id: endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
                    label: "studio".into(),
                    target: "dev@studio.example".into(),
                    session: "agents".into(),
                    enabled: true,
                }]);
                shell.set_endpoint_methods_for(&remote(), Some(vec!["scope.mirror".into()]));
                let fixture = || -> crate::protocol::ClientShellSnapshot {
                    serde_json::from_str(include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/endpoint-snapshot-v1.json"
                    )))
                    .unwrap()
                };
                shell.set_snapshot(Box::new(fixture()));
                shell.set_endpoint_status(
                    &endpoint::ClientEndpointId::Local,
                    endpoint::ClientEndpointStatus::Online,
                );
                let mut studio = fixture();
                studio.boot_id = "studio-boot".into();
                shell.cache_endpoint_snapshot_inactive_for_generation(
                    &remote(),
                    1,
                    Box::new(studio),
                );
                shell.set_endpoint_status(&remote(), endpoint::ClientEndpointStatus::Online);
                (
                    state,
                    endpoints,
                    remote_sent,
                    endpoint_commands::EndpointCommands::default(),
                )
            }

            fn dispatch(
                state: &mut ClientState,
                endpoints: &mut endpoint::EndpointRegistry,
                endpoint_commands: &mut endpoint_commands::EndpointCommands,
            ) {
                dispatch_client_shell_actions(
                    Vec::new(),
                    endpoint_commands,
                    endpoints,
                    state.shell.as_mut(),
                    &mut Vec::new(),
                    &mut None,
                )
                .unwrap();
            }

            /// `(request id, params)` of every mirror studio was sent.
            fn mirrors(sent: &Sent) -> Vec<(String, ScopeMirrorParams)> {
                sent.lock()
                    .unwrap()
                    .iter()
                    .map(|message| {
                        let ClientMessage::ClientShellEndpointRequest { boot_id, request } =
                            message
                        else {
                            panic!("expected an endpoint request, got {message:?}");
                        };
                        assert_eq!(boot_id, "studio-boot");
                        let request: crate::api::schema::Request =
                            serde_json::from_str(request).unwrap();
                        let crate::api::schema::Method::ScopeMirror(params) = request.method else {
                            panic!("expected scope.mirror, got {:?}", request.method);
                        };
                        (request.id, params)
                    })
                    .collect()
            }

            fn project(state: &mut ClientState, scope: ScopeState) {
                route_endpoint_scope_control(
                    state.shell.as_mut().unwrap(),
                    &endpoint::ClientEndpointId::Local,
                    projection(scope),
                )
                .expect("Local projection applies");
            }

            /// Answers studio's in-flight mirror the way the client loop does.
            fn answer(
                state: &mut ClientState,
                endpoint_commands: &mut endpoint_commands::EndpointCommands,
                request_id: &str,
            ) {
                let answer = serde_json::to_vec(&crate::api::schema::SuccessResponse {
                    id: request_id.into(),
                    result: crate::api::schema::ResponseResult::Ok {},
                })
                .unwrap();
                let completed = endpoint_commands
                    .receive_chunk(&remote(), 1, "studio-boot", request_id, true, answer)
                    .unwrap()
                    .expect("the mirror completes");
                let (repaint, actions) =
                    settle_completed_command(state.shell.as_mut().unwrap(), completed, 1);
                assert!(!repaint && actions.is_empty());
            }

            fn switch(
                state: &mut ClientState,
                endpoints: &mut endpoint::EndpointRegistry,
                endpoint_commands: &mut endpoint_commands::EndpointCommands,
                to: &endpoint::ClientEndpointId,
            ) {
                let mut retired = endpoint_commands.retire_lane(&remote());
                retired.extend(endpoint_commands.retire_lane(&endpoint::ClientEndpointId::Local));
                assert!(retired.is_empty(), "scope requests survive: {retired:?}");
                let from = endpoints.active_id().clone();
                assert!(endpoints.set_surface_active(to, true));
                assert!(endpoints.set_active(to));
                assert!(endpoints.set_surface_active(&from, false));
                pump_scope_lanes(endpoint_commands, endpoints, state.shell.as_mut());
            }

            #[test]
            fn a_mirror_rides_an_inactive_remotes_lane_and_survives_machine_switches() {
                let (mut state, mut endpoints, remote_sent, mut endpoint_commands) =
                    presenting_local_beside_studio();
                // Studio is not presented, yet its first mirror leaves at once.
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                let sent = mirrors(&remote_sent);
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].1.machine_label, "studio");
                assert_eq!(sent[0].1.machine, remote().storage_key());

                // A scope change queues behind the mirror still in flight.
                project(&mut state, focused("studio:main"));
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                assert_eq!(mirrors(&remote_sent).len(), 1);

                // Jumping to studio retires both lanes; the mirrors stay.
                switch(
                    &mut state,
                    &mut endpoints,
                    &mut endpoint_commands,
                    &remote(),
                );
                answer(&mut state, &mut endpoint_commands, &sent[0].0);
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                let sent = mirrors(&remote_sent);
                assert_eq!(sent.len(), 2);
                assert_eq!(sent[1].1.scope, Some(focused("studio:main")));

                // Back on Local, studio's lane is inactive again and still takes
                // the next mirror.
                answer(&mut state, &mut endpoint_commands, &sent[1].0);
                switch(
                    &mut state,
                    &mut endpoints,
                    &mut endpoint_commands,
                    &endpoint::ClientEndpointId::Local,
                );
                project(&mut state, focused("studio:ops"));
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                let sent = mirrors(&remote_sent);
                assert_eq!(sent.len(), 3);
                assert_eq!(sent[2].1.scope, Some(focused("studio:ops")));
                assert_eq!(state.shell.as_ref().unwrap().endpoint_notice_text(), None);
            }

            #[test]
            fn a_newer_mirror_supersedes_one_still_queued_on_the_lane() {
                let (mut state, mut endpoints, remote_sent, mut endpoint_commands) =
                    presenting_local_beside_studio();
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                let first = mirrors(&remote_sent).remove(0).0;

                project(&mut state, focused("studio:main"));
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                project(&mut state, focused("studio:ops"));
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                // Both newer mirrors reached the lane behind the first; only the
                // newest is left there.
                assert_eq!(mirrors(&remote_sent).len(), 1);

                answer(&mut state, &mut endpoint_commands, &first);
                dispatch(&mut state, &mut endpoints, &mut endpoint_commands);
                let scopes: Vec<_> = mirrors(&remote_sent)
                    .into_iter()
                    .skip(1)
                    .map(|(_, params)| params.scope)
                    .collect();
                assert_eq!(scopes, [Some(focused("studio:ops"))]);
                // The superseded one settled silently.
                assert_eq!(state.shell.as_ref().unwrap().endpoint_notice_text(), None);
            }
        }
    }
}
