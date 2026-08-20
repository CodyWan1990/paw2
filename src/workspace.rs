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

struct PendingActivation {
    manager: ExtWorkspaceManagerV1,
    output: String,
    workspace: usize,
}

#[derive(Clone)]
pub struct GroupData;

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

    pub fn add_output(&mut self, display: &DisplayHandle, output: &Output, active: usize) {
        self.managers.retain(|instance| instance.manager.is_alive());
        for instance in &mut self.managers {
            let Ok(client) = display.get_client(instance.manager.id()) else {
                continue;
            };
            add_output(display, &client, instance, output, active);
            instance.manager.done();
        }
    }

    pub fn remove_output(&mut self, name: &str) {
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
        GroupData,
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
                let pending = std::mem::take(&mut state.workspace_state.pending);
                let mut requested: Vec<PendingActivation> = Vec::new();
                for request in pending {
                    if request.manager != *manager {
                        state.workspace_state.pending.push(request);
                        continue;
                    }
                    match requested
                        .iter_mut()
                        .find(|item| item.output == request.output)
                    {
                        Some(existing) => *existing = request,
                        None => requested.push(request),
                    }
                }
                for request in requested {
                    state.activate_workspace(&request.output, request.workspace);
                }
            }
            ext_workspace_manager_v1::Request::Stop => manager.finished(),
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, GroupData> for Paw2 {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _data: &GroupData,
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
            state.workspace_state.pending.push(PendingActivation {
                manager: data.manager.clone(),
                output: data.output.clone(),
                workspace: data.workspace,
            });
        }
    }
}
