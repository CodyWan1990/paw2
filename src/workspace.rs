use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State, WorkspaceCapabilities},
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId, protocol::wl_output::WlOutput,
        },
    },
};

use crate::{layout::WORKSPACE_COUNT, state::Paw2};

pub struct WorkspaceState {
    _global: GlobalId,
    managers: Vec<ManagerInstance>,
    pending: Vec<PendingActivation>,
}

struct ManagerInstance {
    manager: ExtWorkspaceManagerV1,
    outputs: Vec<OutputInstance>,
}

struct OutputInstance {
    name: String,
    group: ExtWorkspaceGroupHandleV1,
    workspaces: Vec<ExtWorkspaceHandleV1>,
}

struct PendingActivation<M = ExtWorkspaceManagerV1> {
    manager: M,
    output: String,
    workspace: usize,
}

/// Keeps one pending activation per manager and output: a repeat replaces the
/// earlier request, which is all commit would have kept anyway. Without this a
/// client that activates but never commits grows `pending` without bound.
///
/// Generic over the manager purely so the bound can be unit tested without a
/// Wayland display, for the same reason `Column<T>` in `layout` is generic.
fn record_activation<M: Clone + PartialEq>(
    pending: &mut Vec<PendingActivation<M>>,
    manager: &M,
    output: &str,
    workspace: usize,
) {
    match pending
        .iter_mut()
        .find(|item| item.manager == *manager && item.output == output)
    {
        Some(existing) => existing.workspace = workspace,
        None => pending.push(PendingActivation {
            manager: manager.clone(),
            output: output.into(),
            workspace,
        }),
    }
}

/// Drops every pending activation belonging to `manager`.
///
/// Generic for the same reason [`record_activation`] is: so the `stop` and
/// disconnect paths can be pinned by a test without a Wayland display.
fn forget_manager<M: PartialEq>(pending: &mut Vec<PendingActivation<M>>, manager: &M) {
    pending.retain(|request| request.manager != *manager);
}

#[derive(Clone)]
pub struct WorkspaceData {
    manager: ExtWorkspaceManagerV1,
    output: String,
    workspace: usize,
}

impl WorkspaceState {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            _global: display.create_global::<Paw2, ExtWorkspaceManagerV1, _>(1, ()),
            managers: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn add_manager(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: ExtWorkspaceManagerV1,
        outputs: &[(Output, usize)],
    ) {
        let mut instance = ManagerInstance {
            manager,
            outputs: Vec::new(),
        };
        for (output, active) in outputs {
            add_output(display, client, &mut instance, output, *active);
        }
        instance.manager.done();
        self.managers.push(instance);
    }

    /// Drops managers whose client is gone.
    ///
    /// Every method that touches `managers` calls this first. Binding the global
    /// is unprivileged-adjacent and cheap, while each bind costs a group plus
    /// nine workspace handles *per output*, so pruning in only one place lets a
    /// client grow this list without bound - the same shape as the `pending`
    /// growth fixed in the 2026-08-21 review.
    fn prune(&mut self) {
        self.managers.retain(|instance| instance.manager.is_alive());
        self.pending.retain(|request| request.manager.is_alive());
    }

    pub fn add_output(&mut self, display: &DisplayHandle, output: &Output, active: usize) {
        self.prune();
        for instance in &mut self.managers {
            let Ok(client) = display.get_client(instance.manager.id()) else {
                continue;
            };
            add_output(display, &client, instance, output, active);
            instance.manager.done();
        }
    }

    pub fn remove_output(&mut self, name: &str) {
        self.prune();
        for instance in &mut self.managers {
            let Some(index) = instance
                .outputs
                .iter()
                .position(|output| output.name == name)
            else {
                continue;
            };
            let output = instance.outputs.remove(index);
            for workspace in output.workspaces {
                output.group.workspace_leave(&workspace);
                workspace.removed();
            }
            output.group.removed();
            instance.manager.done();
        }
    }

    pub fn output_bound(&mut self, output: &Output, wl_output: &WlOutput) {
        self.prune();
        for instance in &self.managers {
            if instance.manager.id().same_client_as(&wl_output.id())
                && let Some(group) = instance
                    .outputs
                    .iter()
                    .find(|item| item.name == output.name())
                    .map(|item| &item.group)
            {
                group.output_enter(wl_output);
                instance.manager.done();
            }
        }
    }

    pub fn set_active(&mut self, output: &str, active: usize) {
        self.prune();
        for instance in &self.managers {
            let Some(output) = instance.outputs.iter().find(|item| item.name == output) else {
                continue;
            };
            for (index, workspace) in output.workspaces.iter().enumerate() {
                workspace.state(if index == active {
                    State::Active
                } else {
                    State::empty()
                });
            }
            instance.manager.done();
        }
    }
}

fn add_output(
    display: &DisplayHandle,
    client: &Client,
    instance: &mut ManagerInstance,
    output: &Output,
    active: usize,
) {
    if instance
        .outputs
        .iter()
        .any(|item| item.name == output.name())
    {
        return;
    }
    let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, Paw2>(
        display,
        instance.manager.version(),
        (),
    ) else {
        return;
    };
    instance.manager.workspace_group(&group);
    group.capabilities(GroupCapabilities::empty());
    for wl_output in output.client_outputs(client) {
        group.output_enter(&wl_output);
    }

    let mut workspaces = Vec::with_capacity(WORKSPACE_COUNT);
    for index in 0..WORKSPACE_COUNT {
        let Ok(workspace) = client.create_resource::<ExtWorkspaceHandleV1, _, Paw2>(
            display,
            instance.manager.version(),
            WorkspaceData {
                manager: instance.manager.clone(),
                output: output.name(),
                workspace: index,
            },
        ) else {
            continue;
        };
        instance.manager.workspace(&workspace);
        workspace.id(format!("{}:{}", output.name(), index + 1));
        workspace.name((index + 1).to_string());
        workspace.coordinates((index as u32).to_ne_bytes().to_vec());
        workspace.state(if index == active {
            State::Active
        } else {
            State::empty()
        });
        workspace.capabilities(WorkspaceCapabilities::Activate);
        group.workspace_enter(&workspace);
        workspaces.push(workspace);
    }
    instance.outputs.push(OutputInstance {
        name: output.name(),
        group,
        workspaces,
    });
}

/// Deliberately ungated: this global has no `can_view` filter, and must not grow
/// one.
///
/// Status bars and pagers are the entire point of `ext-workspace-v1`, and they
/// are ordinary unprivileged clients - gating it makes a bar's workspace
/// indicators silently vanish. It also discloses nothing: a workspace handle
/// carries an id, a name, coordinates and an active flag, never window titles or
/// anything else about other clients. The worst a hostile client can do is
/// switch a workspace, which is an annoyance available to any process running as
/// the user by a dozen other routes.
///
/// What bounds the cost of letting everyone bind it is [`WorkspaceState::prune`],
/// not a filter.
impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for Paw2 {
    fn bind(
        state: &mut Self,
        display: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let outputs = state.workspace_outputs();
        state
            .workspace_state
            .add_manager(display, client, manager, &outputs);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let requested = state
                    .workspace_state
                    .pending
                    .extract_if(.., |request| request.manager == *manager)
                    .collect::<Vec<_>>();
                for request in requested {
                    state.activate_workspace(&request.output, request.workspace);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .workspace_state
                    .managers
                    .retain(|instance| instance.manager != *manager);
                forget_manager(&mut state.workspace_state.pending, manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Paw2 {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceData> for Paw2 {
    fn request(
        state: &mut Self,
        _client: &Client,
        _workspace: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, ext_workspace_handle_v1::Request::Activate) {
            record_activation(
                &mut state.workspace_state.pending,
                &data.manager,
                &data.output,
                data.workspace,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_activations_stay_one_per_manager_and_output() {
        let mut pending = Vec::<PendingActivation<u8>>::new();
        for workspace in 0..1_000 {
            record_activation(&mut pending, &1, "DP-1", workspace % WORKSPACE_COUNT);
        }
        record_activation(&mut pending, &1, "DP-2", 3);
        record_activation(&mut pending, &2, "DP-1", 4);

        // A client that never commits cannot grow this past managers x outputs.
        assert_eq!(pending.len(), 3);
        // The last request for a slot is the one that survives.
        assert_eq!(pending[0].workspace, 999 % WORKSPACE_COUNT);

        // Stopping or losing a manager takes its queued requests with it, so a
        // client cannot leave work behind for a manager that no longer exists.
        forget_manager(&mut pending, &1);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].manager, 2);
        forget_manager(&mut pending, &2);
        assert!(pending.is_empty());
    }
}
