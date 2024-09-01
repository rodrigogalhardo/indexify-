use anyhow::{anyhow, Result};
use indexify_internal_api::{
    self as internal_api,
    InvokeComputeGraphPayload,
    StateChange,
    StateChangeId,
    TaskBuilder,
};
use tracing::{error, info};

use crate::{
    state::SharedState,
    task_allocator::{planner::TaskAllocationPlan, TaskAllocator},
};

pub struct Scheduler {
    shared_state: SharedState,
    task_allocator: TaskAllocator,
}

impl Scheduler {
    pub fn new(shared_state: SharedState, task_allocator: TaskAllocator) -> Self {
        Scheduler {
            shared_state,
            task_allocator,
        }
    }

    pub async fn handle_executor_removed(&self, state_change: StateChange) -> Result<()> {
        // This works because when an executor is removed, all its tasks are unassigned.
        let tasks = self.shared_state.unassigned_tasks().await?;
        let plan = self.allocate_tasks(tasks).await?.0;
        if !plan.is_empty() {
            return self
                .shared_state
                .commit_task_assignments(plan, state_change.id)
                .await;
        }

        self.shared_state
            .mark_change_events_as_processed(vec![state_change], Vec::new())
            .await
    }

    pub async fn invoke_compute_graph(
        &self,
        payload: &InvokeComputeGraphPayload,
        state_change_id: StateChangeId,
    ) -> Result<()> {
        let compute_graph = self
            .shared_state
            .state_machine
            .get_compute_graph(payload.namespace.as_str(), payload.graph_name.as_str())?;

        if compute_graph.is_none() {
            error!(
                "compute graph not found: {}/{}",
                payload.namespace, payload.graph_name
            );
            return Ok(());
        }
        let compute_graph = compute_graph.unwrap();
        let task = TaskBuilder::default()
            .namespace(payload.namespace.clone())
            .compute_graph_name(payload.graph_name.clone())
            .compute_fn_name(compute_graph.start_fn.name.clone())
            .input_data_object_id(payload.data_object_id.clone())
            .build()?;
        self.shared_state
            .create_tasks(vec![task], state_change_id)
            .await?;
        Ok(())
    }

    pub async fn allocate_tasks(
        &self,
        tasks: Vec<internal_api::Task>,
    ) -> Result<TaskAllocationPlan> {
        self.task_allocator
            .allocate_tasks(tasks)
            .await
            .map_err(|e| anyhow!("allocate_tasks: {}", e))
    }

    pub async fn redistribute_tasks(&self, _state_change: &StateChange) -> Result<()> {
        // TODO: implement
        Ok(())
    }
}
