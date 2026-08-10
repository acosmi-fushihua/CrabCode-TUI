//! Renderer-only projection of the existing Task tool lifecycle.
//!
//! This module consumes [`ProjectedItem`] values which have already crossed the
//! direct StructuredIO boundary and the SDK projection. It does not read task
//! files, send controls, or define another protocol. Task tool uses and their
//! structured `toolUseResult` values are correlated by `tool_use_id`; display
//! strings are never parsed.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::sdk_projection::{ProjectedItem, ProjectedKind};

const MAX_TASKS: usize = 256;
const MAX_TASK_TOOL_USES: usize = 2_048;
const MAX_TASK_TOOL_RESULTS: usize = 4_096;
const MAX_TASK_ID_BYTES: usize = 512;
const MAX_TASK_SUBJECT_BYTES: usize = 16 * 1024;
const MAX_TASK_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_TASK_ACTIVE_FORM_BYTES: usize = 16 * 1024;
const MAX_TASK_OWNER_BYTES: usize = 4 * 1024;
const MAX_TASK_DEPENDENCIES: usize = 256;

const TASK_CREATE: &str = "TaskCreate";
const TASK_UPDATE: &str = "TaskUpdate";
const TASK_LIST: &str = "TaskList";
const TASK_GET: &str = "TaskGet";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TaskPanelStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskPanelStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskPanelRow {
    pub(crate) id: String,
    pub(crate) subject: String,
    pub(crate) description: Option<String>,
    pub(crate) active_form: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) status: TaskPanelStatus,
    pub(crate) blocks: Vec<String>,
    pub(crate) blocked_by: Vec<String>,
    pub(crate) last_updated_sequence: u64,
}

impl TaskPanelRow {
    pub(crate) fn blocked(&self) -> bool {
        self.status == TaskPanelStatus::Pending && !self.blocked_by.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TaskPanelCounts {
    pub(crate) total: usize,
    pub(crate) pending: usize,
    pub(crate) in_progress: usize,
    pub(crate) completed: usize,
    /// Blocked is a presentation subset of pending, not a fourth wire status.
    pub(crate) blocked: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskPanelSnapshot {
    pub(crate) rows: Vec<TaskPanelRow>,
    pub(crate) counts: TaskPanelCounts,
    pub(crate) source_sequence: Option<u64>,
    /// Result rows whose structured payload was accepted as a successful task
    /// mutation and actually applied to the task-card snapshot. Keys, rather
    /// than just tool-use identities, distinguish a later failed or malformed
    /// replay from an earlier successful result so the later audit row can
    /// never be hidden accidentally.
    successful_mutation_result_keys: HashSet<String>,
}

/// Cached renderer projection keyed by the direct projection's raw-envelope
/// generation. A malformed Task lifecycle keeps the last valid snapshot
/// visible and marks it degraded instead of making the whole panel vanish.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskPanelProjectionState {
    pub(crate) snapshot: Option<Arc<TaskPanelSnapshot>>,
    pub(crate) degraded: bool,
}

#[derive(Debug, Default)]
pub(crate) struct TaskPanelProjectionCache {
    generation: Option<u64>,
    snapshot: Option<Arc<TaskPanelSnapshot>>,
    degraded: bool,
    #[cfg(test)]
    rebuild_count: usize,
}

impl TaskPanelProjectionCache {
    pub(crate) fn project(
        &mut self,
        generation: u64,
        items: &[ProjectedItem],
    ) -> TaskPanelProjectionState {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            #[cfg(test)]
            {
                self.rebuild_count = self.rebuild_count.saturating_add(1);
            }
            match TaskPanelSnapshot::from_projected_items(items) {
                Ok(snapshot) => {
                    self.snapshot = Some(Arc::new(snapshot));
                    self.degraded = false;
                }
                Err(_) => {
                    // Keep the last known-good snapshot. The fixed degraded
                    // marker is rendered separately; untrusted task text or
                    // parser diagnostics never reach the terminal.
                    self.degraded = true;
                }
            }
        }
        TaskPanelProjectionState {
            snapshot: self.snapshot.clone(),
            degraded: self.degraded,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.generation = None;
        self.snapshot = None;
        self.degraded = false;
    }

    #[cfg(test)]
    pub(crate) const fn rebuild_count(&self) -> usize {
        self.rebuild_count
    }
}

impl TaskPanelSnapshot {
    pub(crate) fn from_projected_items(items: &[ProjectedItem]) -> Result<Self, TaskPanelError> {
        snapshot_from_projected_items(items)
    }

    pub(crate) fn has_unfinished(&self) -> bool {
        self.counts.pending != 0 || self.counts.in_progress != 0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn mutation_result_succeeded(&self, item_key: &str) -> bool {
        self.successful_mutation_result_keys.contains(item_key)
    }
}

/// A bounded, non-sensitive failure from a recognized Task tool projection.
///
/// Codes are static so malformed task text or identifiers can never enter a
/// terminal diagnostic through this error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskPanelError {
    pub(crate) sequence: u64,
    pub(crate) code: &'static str,
}

impl TaskPanelError {
    const fn new(sequence: u64, code: &'static str) -> Self {
        Self { sequence, code }
    }
}

impl std::fmt::Display for TaskPanelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "task panel projection failed at sequence {} ({})",
            self.sequence, self.code
        )
    }
}

impl std::error::Error for TaskPanelError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskToolUse {
    Create(CreateInput),
    Update(UpdateInput),
    List,
    Get { task_id: String },
}

impl TaskToolUse {
    const fn name(&self) -> &'static str {
        match self {
            Self::Create(_) => TASK_CREATE,
            Self::Update(_) => TASK_UPDATE,
            Self::List => TASK_LIST,
            Self::Get { .. } => TASK_GET,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskToolUseRecord {
    input: TaskToolUse,
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateInput {
    subject: String,
    description: String,
    active_form: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateInput {
    task_id: String,
    subject: Option<String>,
    description: Option<String>,
    active_form: Option<String>,
    status: Option<UpdateStatus>,
    owner: Option<String>,
    add_blocks: Vec<String>,
    add_blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateStatus {
    Task(TaskPanelStatus),
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequencedOperation {
    tool_use_id: String,
    sequence: u64,
    operation: TaskOperation,
    mutation_result_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskOperation {
    Create(TaskPanelRow),
    Update {
        task_id: String,
        changes: TaskChanges,
    },
    ReplaceList(Vec<TaskPanelRow>),
    Get {
        requested_id: String,
        task: Option<TaskPanelRow>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TaskChanges {
    delete: bool,
    subject: Option<String>,
    description: Option<String>,
    active_form: Option<String>,
    owner: Option<String>,
    status: Option<TaskPanelStatus>,
    add_blocks: Vec<String>,
    add_blocked_by: Vec<String>,
}

pub(crate) fn snapshot_from_projected_items(
    items: &[ProjectedItem],
) -> Result<TaskPanelSnapshot, TaskPanelError> {
    let uses = collect_task_tool_uses(items)?;
    let mut successful_by_tool_use = collect_successful_operations(items, &uses)?
        .into_values()
        .collect::<Vec<_>>();
    successful_by_tool_use.sort_by(|left, right| {
        left.sequence
            .cmp(&right.sequence)
            .then_with(|| left.tool_use_id.cmp(&right.tool_use_id))
    });

    // TaskList is the only complete list snapshot on the existing tool wire.
    // Starting at the latest successful one avoids inventing rows when older
    // TaskCreate events were compacted out of a restored transcript.
    let baseline = successful_by_tool_use
        .iter()
        .rposition(|operation| matches!(operation.operation, TaskOperation::ReplaceList(_)))
        .unwrap_or(0);
    let operations = if successful_by_tool_use.is_empty() {
        &[][..]
    } else {
        &successful_by_tool_use[baseline..]
    };

    let mut rows = HashMap::<String, TaskPanelRow>::new();
    let mut source_sequence = None;
    let mut successful_mutation_result_keys = HashSet::new();
    for operation in operations {
        if apply_operation(&mut rows, operation)?
            && let Some(result_key) = &operation.mutation_result_key
        {
            successful_mutation_result_keys.insert(result_key.clone());
        }
        source_sequence = Some(source_sequence.map_or(operation.sequence, |current: u64| {
            current.max(operation.sequence)
        }));
    }

    let mut rows = rows.into_values().collect::<Vec<_>>();
    rows.sort_by(|left, right| compare_task_ids(&left.id, &right.id));
    let counts = counts_for(&rows);
    Ok(TaskPanelSnapshot {
        rows,
        counts,
        source_sequence,
        successful_mutation_result_keys,
    })
}

fn collect_task_tool_uses(
    items: &[ProjectedItem],
) -> Result<HashMap<String, TaskToolUseRecord>, TaskPanelError> {
    let mut uses: HashMap<String, TaskToolUseRecord> = HashMap::new();
    for item in items {
        if item.kind != ProjectedKind::ToolUse {
            continue;
        }
        let Some(tool) = item.presentation.tool.as_ref() else {
            continue;
        };
        let Some(name) = tool.name.as_deref() else {
            continue;
        };
        if !is_task_tool(name) {
            continue;
        }
        let sequence = item_sequence(item, "task_tool_use_missing_sequence")?;
        let Some(tool_use_id) = item.tool_use_id.as_deref() else {
            return Err(TaskPanelError::new(sequence, "task_tool_use_missing_id"));
        };
        validate_bounded_nonempty(
            tool_use_id,
            MAX_TASK_ID_BYTES,
            sequence,
            "task_tool_use_id_invalid",
        )?;
        let Some(input) = tool.input.as_ref() else {
            if item.streaming || tool.partial_input_json.is_some() {
                continue;
            }
            return Err(TaskPanelError::new(sequence, "task_tool_use_missing_input"));
        };
        let parsed = parse_task_tool_use(name, input, sequence)?;
        let record = TaskToolUseRecord {
            input: parsed,
            sequence,
        };
        if let Some(previous) = uses.get(tool_use_id) {
            if previous.input != record.input {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_tool_use_conflicting_replay",
                ));
            }
            if record.sequence > previous.sequence {
                uses.insert(tool_use_id.to_string(), record);
            }
        } else {
            if uses.len() >= MAX_TASK_TOOL_USES {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_tool_use_limit_exceeded",
                ));
            }
            uses.insert(tool_use_id.to_string(), record);
        }
    }
    Ok(uses)
}

fn collect_successful_operations(
    items: &[ProjectedItem],
    uses: &HashMap<String, TaskToolUseRecord>,
) -> Result<HashMap<String, SequencedOperation>, TaskPanelError> {
    let mut result_count = 0_usize;
    let mut latest = HashMap::<String, SequencedOperation>::new();
    for item in items {
        if item.kind != ProjectedKind::ToolResult {
            continue;
        }
        let Some(tool) = item.presentation.tool.as_ref() else {
            continue;
        };
        let recognized_result_name = tool.name.as_deref().filter(|name| is_task_tool(name));
        let Some(tool_use_id) = item.tool_use_id.as_deref() else {
            if recognized_result_name.is_some() {
                let sequence = item_sequence(item, "task_tool_result_missing_sequence")?;
                return Err(TaskPanelError::new(sequence, "task_tool_result_missing_id"));
            }
            continue;
        };
        let Some(task_use) = uses.get(tool_use_id) else {
            if recognized_result_name.is_some() {
                let sequence = item_sequence(item, "task_tool_result_missing_sequence")?;
                return Err(TaskPanelError::new(
                    sequence,
                    "task_tool_result_without_use",
                ));
            }
            continue;
        };
        let sequence = item_sequence(item, "task_tool_result_missing_sequence")?;
        if let Some(result_name) = recognized_result_name
            && result_name != task_use.input.name()
        {
            return Err(TaskPanelError::new(
                sequence,
                "task_tool_result_name_mismatch",
            ));
        }
        result_count = result_count.saturating_add(1);
        if result_count > MAX_TASK_TOOL_RESULTS {
            return Err(TaskPanelError::new(
                sequence,
                "task_tool_result_limit_exceeded",
            ));
        }
        if tool.is_error == Some(true) {
            continue;
        }
        let Some(result) = tool.result.as_ref() else {
            return Err(TaskPanelError::new(
                sequence,
                "task_tool_result_missing_payload",
            ));
        };
        let Some(operation) = parse_successful_operation(&task_use.input, result, sequence)? else {
            continue;
        };
        let candidate = SequencedOperation {
            tool_use_id: tool_use_id.to_string(),
            sequence,
            operation,
            mutation_result_key: matches!(
                &task_use.input,
                TaskToolUse::Create(_) | TaskToolUse::Update(_)
            )
            .then(|| item.key.clone()),
        };
        match latest.get(tool_use_id) {
            Some(previous) if previous.sequence > candidate.sequence => {}
            Some(previous)
                if previous.sequence == candidate.sequence
                    && previous.operation != candidate.operation =>
            {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_tool_result_conflicting_replay",
                ));
            }
            _ => {
                latest.insert(tool_use_id.to_string(), candidate);
            }
        }
    }
    Ok(latest)
}

fn parse_task_tool_use(
    name: &str,
    input: &Value,
    sequence: u64,
) -> Result<TaskToolUse, TaskPanelError> {
    let object = object(input, sequence, "task_tool_input_not_object")?;
    match name {
        TASK_CREATE => {
            exact_fields(
                object,
                &["subject", "description"],
                &["activeForm", "metadata"],
                sequence,
                "task_create_input_fields_invalid",
            )?;
            if object
                .get("metadata")
                .is_some_and(|metadata| !metadata.is_object())
            {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_create_metadata_invalid",
                ));
            }
            Ok(TaskToolUse::Create(CreateInput {
                subject: bounded_string_field(
                    object,
                    "subject",
                    MAX_TASK_SUBJECT_BYTES,
                    sequence,
                    "task_create_subject_invalid",
                )?,
                description: bounded_string_field(
                    object,
                    "description",
                    MAX_TASK_DESCRIPTION_BYTES,
                    sequence,
                    "task_create_description_invalid",
                )?,
                active_form: optional_bounded_string_field(
                    object,
                    "activeForm",
                    MAX_TASK_ACTIVE_FORM_BYTES,
                    sequence,
                    "task_create_active_form_invalid",
                )?,
            }))
        }
        TASK_UPDATE => {
            exact_fields(
                object,
                &["taskId"],
                &[
                    "subject",
                    "description",
                    "activeForm",
                    "status",
                    "addBlocks",
                    "addBlockedBy",
                    "owner",
                    "metadata",
                ],
                sequence,
                "task_update_input_fields_invalid",
            )?;
            if object
                .get("metadata")
                .is_some_and(|metadata| !metadata.is_object())
            {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_update_metadata_invalid",
                ));
            }
            Ok(TaskToolUse::Update(UpdateInput {
                task_id: bounded_id_field(object, "taskId", sequence, "task_update_id_invalid")?,
                subject: optional_bounded_string_field(
                    object,
                    "subject",
                    MAX_TASK_SUBJECT_BYTES,
                    sequence,
                    "task_update_subject_invalid",
                )?,
                description: optional_bounded_string_field(
                    object,
                    "description",
                    MAX_TASK_DESCRIPTION_BYTES,
                    sequence,
                    "task_update_description_invalid",
                )?,
                active_form: optional_bounded_string_field(
                    object,
                    "activeForm",
                    MAX_TASK_ACTIVE_FORM_BYTES,
                    sequence,
                    "task_update_active_form_invalid",
                )?,
                status: object
                    .get("status")
                    .map(|status| parse_update_status(status, sequence))
                    .transpose()?,
                owner: optional_bounded_string_field(
                    object,
                    "owner",
                    MAX_TASK_OWNER_BYTES,
                    sequence,
                    "task_update_owner_invalid",
                )?,
                add_blocks: optional_id_array(
                    object,
                    "addBlocks",
                    sequence,
                    "task_update_add_blocks_invalid",
                )?,
                add_blocked_by: optional_id_array(
                    object,
                    "addBlockedBy",
                    sequence,
                    "task_update_add_blocked_by_invalid",
                )?,
            }))
        }
        TASK_LIST => {
            exact_fields(object, &[], &[], sequence, "task_list_input_fields_invalid")?;
            Ok(TaskToolUse::List)
        }
        TASK_GET => {
            exact_fields(
                object,
                &["taskId"],
                &[],
                sequence,
                "task_get_input_fields_invalid",
            )?;
            Ok(TaskToolUse::Get {
                task_id: bounded_id_field(object, "taskId", sequence, "task_get_id_invalid")?,
            })
        }
        _ => unreachable!("the caller selected an exact Task tool name"),
    }
}

fn parse_successful_operation(
    task_use: &TaskToolUse,
    result: &Value,
    sequence: u64,
) -> Result<Option<TaskOperation>, TaskPanelError> {
    match task_use {
        TaskToolUse::Create(input) => parse_create_result(input, result, sequence).map(Some),
        TaskToolUse::Update(input) => parse_update_result(input, result, sequence),
        TaskToolUse::List => parse_list_result(result, sequence).map(Some),
        TaskToolUse::Get { task_id } => parse_get_result(task_id, result, sequence).map(Some),
    }
}

fn parse_create_result(
    input: &CreateInput,
    result: &Value,
    sequence: u64,
) -> Result<TaskOperation, TaskPanelError> {
    let result = object(result, sequence, "task_create_result_not_object")?;
    exact_fields(
        result,
        &["task"],
        &[],
        sequence,
        "task_create_result_fields_invalid",
    )?;
    let task = result
        .get("task")
        .and_then(Value::as_object)
        .ok_or_else(|| TaskPanelError::new(sequence, "task_create_result_task_invalid"))?;
    exact_fields(
        task,
        &["id", "subject"],
        &[],
        sequence,
        "task_create_result_task_fields_invalid",
    )?;
    let id = bounded_id_field(task, "id", sequence, "task_create_result_id_invalid")?;
    let subject = bounded_string_field(
        task,
        "subject",
        MAX_TASK_SUBJECT_BYTES,
        sequence,
        "task_create_result_subject_invalid",
    )?;
    if subject != input.subject {
        return Err(TaskPanelError::new(
            sequence,
            "task_create_subject_mismatch",
        ));
    }
    Ok(TaskOperation::Create(TaskPanelRow {
        id,
        subject,
        description: Some(input.description.clone()),
        active_form: input.active_form.clone(),
        owner: None,
        status: TaskPanelStatus::Pending,
        blocks: Vec::new(),
        blocked_by: Vec::new(),
        last_updated_sequence: sequence,
    }))
}

fn parse_update_result(
    input: &UpdateInput,
    result: &Value,
    sequence: u64,
) -> Result<Option<TaskOperation>, TaskPanelError> {
    let result = object(result, sequence, "task_update_result_not_object")?;
    exact_fields(
        result,
        &["success", "taskId", "updatedFields"],
        &["error", "statusChange", "verificationNudgeNeeded"],
        sequence,
        "task_update_result_fields_invalid",
    )?;
    let success = result
        .get("success")
        .and_then(Value::as_bool)
        .ok_or_else(|| TaskPanelError::new(sequence, "task_update_success_invalid"))?;
    let task_id = bounded_id_field(result, "taskId", sequence, "task_update_result_id_invalid")?;
    if task_id != input.task_id {
        return Err(TaskPanelError::new(
            sequence,
            "task_update_result_id_mismatch",
        ));
    }
    if result.get("error").is_some_and(|error| !error.is_string())
        || result
            .get("verificationNudgeNeeded")
            .is_some_and(|nudge| !nudge.is_boolean())
    {
        return Err(TaskPanelError::new(
            sequence,
            "task_update_optional_result_field_invalid",
        ));
    }
    let updated_fields = string_set_field(
        result,
        "updatedFields",
        sequence,
        "task_update_updated_fields_invalid",
    )?;
    for field in &updated_fields {
        if !matches!(
            field.as_str(),
            "subject"
                | "description"
                | "activeForm"
                | "status"
                | "owner"
                | "metadata"
                | "blocks"
                | "blockedBy"
                | "deleted"
        ) {
            return Err(TaskPanelError::new(
                sequence,
                "task_update_unknown_updated_field",
            ));
        }
    }
    if !success {
        return Ok(None);
    }

    let status_change = result.get("statusChange");
    let changes_status = updated_fields.contains("status") || updated_fields.contains("deleted");
    if status_change.is_some() != changes_status {
        return Err(TaskPanelError::new(
            sequence,
            "task_update_status_change_presence_mismatch",
        ));
    }
    let mut changes = TaskChanges::default();
    if updated_fields.contains("subject") {
        changes.subject = Some(input.subject.clone().ok_or_else(|| {
            TaskPanelError::new(sequence, "task_update_confirmed_subject_missing_input")
        })?);
    }
    if updated_fields.contains("description") {
        changes.description = Some(input.description.clone().ok_or_else(|| {
            TaskPanelError::new(sequence, "task_update_confirmed_description_missing_input")
        })?);
    }
    if updated_fields.contains("activeForm") {
        changes.active_form = Some(input.active_form.clone().ok_or_else(|| {
            TaskPanelError::new(sequence, "task_update_confirmed_active_form_missing_input")
        })?);
    }
    if updated_fields.contains("owner") {
        // Swarm mode can auto-assign the current agent while the input omits
        // owner. The result confirms the field changed but does not expose the
        // exact owner, so leave it unchanged instead of inventing a value.
        changes.owner.clone_from(&input.owner);
    }
    if updated_fields.contains("blocks") {
        if input.add_blocks.is_empty() {
            return Err(TaskPanelError::new(
                sequence,
                "task_update_confirmed_blocks_missing_input",
            ));
        }
        changes.add_blocks.clone_from(&input.add_blocks);
    }
    if updated_fields.contains("blockedBy") {
        if input.add_blocked_by.is_empty() {
            return Err(TaskPanelError::new(
                sequence,
                "task_update_confirmed_blocked_by_missing_input",
            ));
        }
        changes.add_blocked_by.clone_from(&input.add_blocked_by);
    }
    if let Some(status_change) = status_change {
        let status_change = status_change
            .as_object()
            .ok_or_else(|| TaskPanelError::new(sequence, "task_update_status_change_invalid"))?;
        exact_fields(
            status_change,
            &["from", "to"],
            &[],
            sequence,
            "task_update_status_change_fields_invalid",
        )?;
        parse_task_status_field(
            status_change,
            "from",
            sequence,
            "task_update_status_change_from_invalid",
        )?;
        let to = status_change
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| TaskPanelError::new(sequence, "task_update_status_change_to_invalid"))?;
        match input.status {
            Some(UpdateStatus::Deleted)
                if updated_fields.contains("deleted") && to == "deleted" =>
            {
                changes.delete = true;
            }
            Some(UpdateStatus::Task(expected))
                if updated_fields.contains("status") && to == expected.as_str() =>
            {
                changes.status = Some(expected);
            }
            _ => {
                return Err(TaskPanelError::new(
                    sequence,
                    "task_update_status_change_mismatch",
                ));
            }
        }
    }
    Ok(Some(TaskOperation::Update { task_id, changes }))
}

fn parse_list_result(result: &Value, sequence: u64) -> Result<TaskOperation, TaskPanelError> {
    let result = object(result, sequence, "task_list_result_not_object")?;
    exact_fields(
        result,
        &["tasks"],
        &[],
        sequence,
        "task_list_result_fields_invalid",
    )?;
    let tasks = result
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| TaskPanelError::new(sequence, "task_list_tasks_invalid"))?;
    if tasks.len() > MAX_TASKS {
        return Err(TaskPanelError::new(
            sequence,
            "task_list_task_limit_exceeded",
        ));
    }
    let mut seen = HashSet::new();
    let mut rows = Vec::with_capacity(tasks.len());
    for task in tasks {
        let task = task
            .as_object()
            .ok_or_else(|| TaskPanelError::new(sequence, "task_list_task_invalid"))?;
        exact_fields(
            task,
            &["id", "subject", "status", "blockedBy"],
            &["owner"],
            sequence,
            "task_list_task_fields_invalid",
        )?;
        let id = bounded_id_field(task, "id", sequence, "task_list_task_id_invalid")?;
        if !seen.insert(id.clone()) {
            return Err(TaskPanelError::new(sequence, "task_list_duplicate_id"));
        }
        rows.push(TaskPanelRow {
            id,
            subject: bounded_string_field(
                task,
                "subject",
                MAX_TASK_SUBJECT_BYTES,
                sequence,
                "task_list_task_subject_invalid",
            )?,
            description: None,
            active_form: None,
            owner: optional_bounded_string_field(
                task,
                "owner",
                MAX_TASK_OWNER_BYTES,
                sequence,
                "task_list_task_owner_invalid",
            )?,
            status: parse_task_status_field(
                task,
                "status",
                sequence,
                "task_list_task_status_invalid",
            )?,
            blocks: Vec::new(),
            blocked_by: id_array_field(
                task,
                "blockedBy",
                sequence,
                "task_list_task_blocked_by_invalid",
            )?,
            last_updated_sequence: sequence,
        });
    }
    Ok(TaskOperation::ReplaceList(rows))
}

fn parse_get_result(
    requested_id: &str,
    result: &Value,
    sequence: u64,
) -> Result<TaskOperation, TaskPanelError> {
    let result = object(result, sequence, "task_get_result_not_object")?;
    exact_fields(
        result,
        &["task"],
        &[],
        sequence,
        "task_get_result_fields_invalid",
    )?;
    let Some(task) = result.get("task") else {
        unreachable!("exact_fields required task")
    };
    if task.is_null() {
        return Ok(TaskOperation::Get {
            requested_id: requested_id.to_string(),
            task: None,
        });
    }
    let task = task
        .as_object()
        .ok_or_else(|| TaskPanelError::new(sequence, "task_get_task_invalid"))?;
    exact_fields(
        task,
        &[
            "id",
            "subject",
            "description",
            "status",
            "blocks",
            "blockedBy",
        ],
        &[],
        sequence,
        "task_get_task_fields_invalid",
    )?;
    let id = bounded_id_field(task, "id", sequence, "task_get_task_id_invalid")?;
    if id != requested_id {
        return Err(TaskPanelError::new(sequence, "task_get_result_id_mismatch"));
    }
    Ok(TaskOperation::Get {
        requested_id: requested_id.to_string(),
        task: Some(TaskPanelRow {
            id,
            subject: bounded_string_field(
                task,
                "subject",
                MAX_TASK_SUBJECT_BYTES,
                sequence,
                "task_get_task_subject_invalid",
            )?,
            description: Some(bounded_string_field(
                task,
                "description",
                MAX_TASK_DESCRIPTION_BYTES,
                sequence,
                "task_get_task_description_invalid",
            )?),
            active_form: None,
            owner: None,
            status: parse_task_status_field(
                task,
                "status",
                sequence,
                "task_get_task_status_invalid",
            )?,
            blocks: id_array_field(task, "blocks", sequence, "task_get_task_blocks_invalid")?,
            blocked_by: id_array_field(
                task,
                "blockedBy",
                sequence,
                "task_get_task_blocked_by_invalid",
            )?,
            last_updated_sequence: sequence,
        }),
    })
}

fn apply_operation(
    rows: &mut HashMap<String, TaskPanelRow>,
    operation: &SequencedOperation,
) -> Result<bool, TaskPanelError> {
    match &operation.operation {
        TaskOperation::Create(task) => {
            if let Some(existing) = rows.get(&task.id)
                && existing.subject != task.subject
            {
                return Err(TaskPanelError::new(
                    operation.sequence,
                    "task_create_id_conflict",
                ));
            }
            if !rows.contains_key(&task.id) && rows.len() >= MAX_TASKS {
                return Err(TaskPanelError::new(
                    operation.sequence,
                    "task_panel_task_limit_exceeded",
                ));
            }
            rows.insert(task.id.clone(), task.clone());
            return Ok(true);
        }
        TaskOperation::Update { task_id, changes } => {
            if changes.delete {
                return Ok(rows.remove(task_id).is_some());
            }
            // A compacted history can retain an update after its create row was
            // removed. Without a TaskList/TaskGet subject and status, creating a
            // placeholder would invent display state, so ignore it fail-closed.
            let Some(task) = rows.get_mut(task_id) else {
                return Ok(false);
            };
            if let Some(subject) = &changes.subject {
                task.subject.clone_from(subject);
            }
            if let Some(description) = &changes.description {
                task.description = Some(description.clone());
            }
            if let Some(active_form) = &changes.active_form {
                task.active_form = Some(active_form.clone());
            }
            if let Some(owner) = &changes.owner {
                task.owner = Some(owner.clone());
            }
            if let Some(status) = changes.status {
                task.status = status;
            }
            extend_unique_bounded(&mut task.blocks, &changes.add_blocks, operation.sequence)?;
            extend_unique_bounded(
                &mut task.blocked_by,
                &changes.add_blocked_by,
                operation.sequence,
            )?;
            task.last_updated_sequence = operation.sequence;
            return Ok(true);
        }
        TaskOperation::ReplaceList(tasks) => {
            rows.clear();
            for task in tasks {
                rows.insert(task.id.clone(), task.clone());
            }
        }
        TaskOperation::Get { requested_id, task } => match task {
            Some(task) => {
                if !rows.contains_key(requested_id) && rows.len() >= MAX_TASKS {
                    return Err(TaskPanelError::new(
                        operation.sequence,
                        "task_panel_task_limit_exceeded",
                    ));
                }
                if let Some(existing) = rows.get(requested_id) {
                    let mut merged = task.clone();
                    merged.active_form.clone_from(&existing.active_form);
                    merged.owner.clone_from(&existing.owner);
                    rows.insert(requested_id.clone(), merged);
                } else {
                    rows.insert(requested_id.clone(), task.clone());
                }
            }
            None => {
                rows.remove(requested_id);
            }
        },
    }
    Ok(false)
}

fn counts_for(rows: &[TaskPanelRow]) -> TaskPanelCounts {
    let mut counts = TaskPanelCounts {
        total: rows.len(),
        ..TaskPanelCounts::default()
    };
    for row in rows {
        match row.status {
            TaskPanelStatus::Pending => counts.pending = counts.pending.saturating_add(1),
            TaskPanelStatus::InProgress => {
                counts.in_progress = counts.in_progress.saturating_add(1);
            }
            TaskPanelStatus::Completed => {
                counts.completed = counts.completed.saturating_add(1);
            }
        }
        if row.blocked() {
            counts.blocked = counts.blocked.saturating_add(1);
        }
    }
    counts
}

fn compare_task_ids(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left_number), Ok(right_number)) => left_number
            .cmp(&right_number)
            .then_with(|| left.len().cmp(&right.len()))
            .then_with(|| left.cmp(right)),
        _ => left.cmp(right),
    }
}

fn extend_unique_bounded(
    destination: &mut Vec<String>,
    additions: &[String],
    sequence: u64,
) -> Result<(), TaskPanelError> {
    for addition in additions {
        if destination.iter().any(|known| known == addition) {
            continue;
        }
        if destination.len() >= MAX_TASK_DEPENDENCIES {
            return Err(TaskPanelError::new(
                sequence,
                "task_dependency_limit_exceeded",
            ));
        }
        destination.push(addition.clone());
    }
    Ok(())
}

fn item_sequence(item: &ProjectedItem, code: &'static str) -> Result<u64, TaskPanelError> {
    item.raw_sequences
        .iter()
        .copied()
        .max()
        .ok_or_else(|| TaskPanelError::new(0, code))
}

fn is_task_tool(name: &str) -> bool {
    matches!(name, TASK_CREATE | TASK_UPDATE | TASK_LIST | TASK_GET)
}

fn parse_update_status(value: &Value, sequence: u64) -> Result<UpdateStatus, TaskPanelError> {
    match value.as_str() {
        Some("pending") => Ok(UpdateStatus::Task(TaskPanelStatus::Pending)),
        Some("in_progress") => Ok(UpdateStatus::Task(TaskPanelStatus::InProgress)),
        Some("completed") => Ok(UpdateStatus::Task(TaskPanelStatus::Completed)),
        Some("deleted") => Ok(UpdateStatus::Deleted),
        _ => Err(TaskPanelError::new(sequence, "task_update_status_invalid")),
    }
}

fn parse_task_status_field(
    object: &Map<String, Value>,
    field: &str,
    sequence: u64,
    code: &'static str,
) -> Result<TaskPanelStatus, TaskPanelError> {
    match object.get(field).and_then(Value::as_str) {
        Some("pending") => Ok(TaskPanelStatus::Pending),
        Some("in_progress") => Ok(TaskPanelStatus::InProgress),
        Some("completed") => Ok(TaskPanelStatus::Completed),
        _ => Err(TaskPanelError::new(sequence, code)),
    }
}

fn object<'a>(
    value: &'a Value,
    sequence: u64,
    code: &'static str,
) -> Result<&'a Map<String, Value>, TaskPanelError> {
    value
        .as_object()
        .ok_or_else(|| TaskPanelError::new(sequence, code))
}

fn exact_fields(
    object: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    sequence: u64,
    code: &'static str,
) -> Result<(), TaskPanelError> {
    if required.iter().any(|field| !object.contains_key(*field))
        || object
            .keys()
            .any(|field| !required.contains(&field.as_str()) && !optional.contains(&field.as_str()))
    {
        return Err(TaskPanelError::new(sequence, code));
    }
    Ok(())
}

fn bounded_id_field(
    object: &Map<String, Value>,
    field: &str,
    sequence: u64,
    code: &'static str,
) -> Result<String, TaskPanelError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TaskPanelError::new(sequence, code))?;
    validate_bounded_nonempty(value, MAX_TASK_ID_BYTES, sequence, code)?;
    Ok(value.to_string())
}

fn bounded_string_field(
    object: &Map<String, Value>,
    field: &str,
    max_bytes: usize,
    sequence: u64,
    code: &'static str,
) -> Result<String, TaskPanelError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TaskPanelError::new(sequence, code))?;
    if value.len() > max_bytes {
        return Err(TaskPanelError::new(sequence, code));
    }
    Ok(value.to_string())
}

fn optional_bounded_string_field(
    object: &Map<String, Value>,
    field: &str,
    max_bytes: usize,
    sequence: u64,
    code: &'static str,
) -> Result<Option<String>, TaskPanelError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| TaskPanelError::new(sequence, code))?;
    if value.len() > max_bytes {
        return Err(TaskPanelError::new(sequence, code));
    }
    Ok(Some(value.to_string()))
}

fn validate_bounded_nonempty(
    value: &str,
    max_bytes: usize,
    sequence: u64,
    code: &'static str,
) -> Result<(), TaskPanelError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(TaskPanelError::new(sequence, code));
    }
    Ok(())
}

fn optional_id_array(
    object: &Map<String, Value>,
    field: &str,
    sequence: u64,
    code: &'static str,
) -> Result<Vec<String>, TaskPanelError> {
    match object.get(field) {
        None => Ok(Vec::new()),
        Some(_) => id_array_field(object, field, sequence, code),
    }
}

fn id_array_field(
    object: &Map<String, Value>,
    field: &str,
    sequence: u64,
    code: &'static str,
) -> Result<Vec<String>, TaskPanelError> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| TaskPanelError::new(sequence, code))?;
    if values.len() > MAX_TASK_DEPENDENCIES {
        return Err(TaskPanelError::new(sequence, code));
    }
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| TaskPanelError::new(sequence, code))?;
        validate_bounded_nonempty(value, MAX_TASK_ID_BYTES, sequence, code)?;
        if !seen.insert(value) {
            return Err(TaskPanelError::new(sequence, code));
        }
        result.push(value.to_string());
    }
    Ok(result)
}

fn string_set_field(
    object: &Map<String, Value>,
    field: &str,
    sequence: u64,
    code: &'static str,
) -> Result<HashSet<String>, TaskPanelError> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| TaskPanelError::new(sequence, code))?;
    if values.len() > 16 {
        return Err(TaskPanelError::new(sequence, code));
    }
    let mut result = HashSet::with_capacity(values.len());
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| TaskPanelError::new(sequence, code))?;
        if !result.insert(value.to_string()) {
            return Err(TaskPanelError::new(sequence, code));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sdk_projection::{ProjectedPresentation, ToolPresentation};

    fn tool_use(sequence: u64, id: &str, name: &str, input: Value) -> ProjectedItem {
        ProjectedItem {
            key: format!("use-{sequence}-{id}"),
            kind: ProjectedKind::ToolUse,
            title: name.to_string(),
            text: String::new(),
            streaming: false,
            raw_sequences: vec![sequence],
            tool_use_id: Some(id.to_string()),
            presentation: ProjectedPresentation {
                tool: Some(ToolPresentation {
                    name: Some(name.to_string()),
                    input: Some(input),
                    partial_input_json: None,
                    lifecycle_output: None,
                    result: None,
                    is_error: None,
                }),
                ..ProjectedPresentation::default()
            },
        }
    }

    fn tool_result(sequence: u64, id: &str, name: &str, result: Value) -> ProjectedItem {
        ProjectedItem {
            key: format!("result-{sequence}-{id}"),
            kind: ProjectedKind::ToolResult,
            title: format!("{name} result"),
            text: String::new(),
            streaming: false,
            raw_sequences: vec![sequence],
            tool_use_id: Some(id.to_string()),
            presentation: ProjectedPresentation {
                tool: Some(ToolPresentation {
                    name: Some(name.to_string()),
                    input: None,
                    partial_input_json: None,
                    lifecycle_output: None,
                    result: Some(result),
                    is_error: Some(false),
                }),
                ..ProjectedPresentation::default()
            },
        }
    }

    fn create_use(sequence: u64, call_id: &str, subject: &str) -> ProjectedItem {
        tool_use(
            sequence,
            call_id,
            TASK_CREATE,
            json!({
                "subject":subject,
                "description":format!("Description for {subject}"),
                "activeForm":format!("Running {subject}")
            }),
        )
    }

    fn create_result(sequence: u64, call_id: &str, task_id: &str, subject: &str) -> ProjectedItem {
        tool_result(
            sequence,
            call_id,
            TASK_CREATE,
            json!({"task":{"id":task_id,"subject":subject}}),
        )
    }

    #[test]
    fn create_results_correlate_by_tool_use_id_and_sort_numeric_task_ids() {
        let items = vec![
            // Results deliberately precede their uses in the input slice and
            // arrive in the opposite order from numeric task identity.
            create_result(20, "call-b", "10", "ten"),
            create_result(21, "call-a", "2", "two"),
            create_use(1, "call-a", "two"),
            create_use(2, "call-b", "ten"),
        ];
        let snapshot = TaskPanelSnapshot::from_projected_items(&items).expect("task snapshot");

        assert_eq!(
            snapshot
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["2", "10"]
        );
        assert_eq!(snapshot.counts.total, 2);
        assert_eq!(snapshot.counts.pending, 2);
        assert!(snapshot.has_unfinished());
        assert_eq!(snapshot.source_sequence, Some(21));
        assert_eq!(snapshot.rows[0].active_form.as_deref(), Some("Running two"));
    }

    #[test]
    fn only_successful_confirmed_updates_mutate_task_state() {
        let mut failed_result = tool_result(
            4,
            "update-failed",
            TASK_UPDATE,
            json!({
                "success":false,
                "taskId":"1",
                "updatedFields":[],
                "error":"Task not found"
            }),
        );
        failed_result
            .presentation
            .tool
            .as_mut()
            .expect("tool")
            .is_error = None;
        let items = vec![
            create_use(1, "create", "subject"),
            create_result(2, "create", "1", "subject"),
            tool_use(
                3,
                "update-failed",
                TASK_UPDATE,
                json!({"taskId":"1","status":"completed"}),
            ),
            failed_result,
            tool_use(
                5,
                "update-ok",
                TASK_UPDATE,
                json!({"taskId":"1","status":"in_progress"}),
            ),
            tool_result(
                6,
                "update-ok",
                TASK_UPDATE,
                json!({
                    "success":true,
                    "taskId":"1",
                    "updatedFields":["status"],
                    "statusChange":{"from":"pending","to":"in_progress"},
                    "verificationNudgeNeeded":false
                }),
            ),
        ];
        let snapshot = snapshot_from_projected_items(&items).expect("task snapshot");

        assert_eq!(snapshot.rows[0].status, TaskPanelStatus::InProgress);
        assert_eq!(snapshot.counts.in_progress, 1);
        assert_eq!(snapshot.counts.completed, 0);
        assert!(snapshot.mutation_result_succeeded("result-2-create"));
        assert!(snapshot.mutation_result_succeeded("result-6-update-ok"));
        assert!(!snapshot.mutation_result_succeeded("result-4-update-failed"));
    }

    #[test]
    fn latest_task_list_is_authoritative_and_later_updates_win() {
        let items = vec![
            create_use(1, "old-create", "not in snapshot"),
            create_result(2, "old-create", "9", "not in snapshot"),
            tool_use(3, "list", TASK_LIST, json!({})),
            tool_result(
                4,
                "list",
                TASK_LIST,
                json!({"tasks":[
                    {"id":"1","subject":"first","status":"pending","blockedBy":[]},
                    {"id":"2","subject":"second","status":"pending","owner":"agent-a","blockedBy":["1"]}
                ]}),
            ),
            tool_use(
                5,
                "update",
                TASK_UPDATE,
                json!({"taskId":"1","status":"completed"}),
            ),
            tool_result(
                6,
                "update",
                TASK_UPDATE,
                json!({
                    "success":true,
                    "taskId":"1",
                    "updatedFields":["status"],
                    "statusChange":{"from":"pending","to":"completed"}
                }),
            ),
        ];
        let snapshot = snapshot_from_projected_items(&items).expect("task snapshot");

        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[0].status, TaskPanelStatus::Completed);
        assert!(snapshot.rows[1].blocked());
        assert_eq!(snapshot.rows[1].owner.as_deref(), Some("agent-a"));
        assert_eq!(snapshot.counts.blocked, 1);
        assert_eq!(snapshot.counts.completed, 1);
        assert!(snapshot.mutation_result_succeeded("result-6-update"));
        assert!(
            !snapshot.mutation_result_succeeded("result-2-old-create"),
            "a pre-baseline mutation was not applied to the current card"
        );
    }

    #[test]
    fn successful_update_without_a_card_row_is_not_marked_as_compactable() {
        let items = vec![
            tool_use(
                1,
                "missing-update",
                TASK_UPDATE,
                json!({"taskId":"404","status":"in_progress"}),
            ),
            tool_result(
                2,
                "missing-update",
                TASK_UPDATE,
                json!({
                    "success":true,
                    "taskId":"404",
                    "updatedFields":["status"],
                    "statusChange":{"from":"pending","to":"in_progress"}
                }),
            ),
        ];
        let snapshot = snapshot_from_projected_items(&items).expect("task snapshot");

        assert!(snapshot.is_empty());
        assert!(!snapshot.mutation_result_succeeded("result-2-missing-update"));
    }

    #[test]
    fn task_get_fills_details_and_null_result_removes_the_row() {
        let list_use = tool_use(1, "list", TASK_LIST, json!({}));
        let list_result = tool_result(
            2,
            "list",
            TASK_LIST,
            json!({"tasks":[{"id":"1","subject":"one","status":"pending","owner":"agent","blockedBy":[]}]}),
        );
        let get_use = tool_use(3, "get", TASK_GET, json!({"taskId":"1"}));
        let get_result = tool_result(
            4,
            "get",
            TASK_GET,
            json!({"task":{
                "id":"1","subject":"one","description":"details","status":"in_progress",
                "blocks":["2"],"blockedBy":[]
            }}),
        );
        let first = snapshot_from_projected_items(&[
            list_use.clone(),
            list_result.clone(),
            get_use,
            get_result,
        ])
        .expect("task snapshot");
        assert_eq!(first.rows[0].description.as_deref(), Some("details"));
        assert_eq!(first.rows[0].owner.as_deref(), Some("agent"));
        assert_eq!(first.rows[0].blocks, ["2"]);

        let removed = snapshot_from_projected_items(&[
            list_use,
            list_result,
            tool_use(5, "missing", TASK_GET, json!({"taskId":"1"})),
            tool_result(6, "missing", TASK_GET, json!({"task":null})),
        ])
        .expect("task snapshot");
        assert!(removed.is_empty());
    }

    #[test]
    fn latest_successful_result_for_one_tool_use_id_wins() {
        let items = vec![
            tool_use(1, "get", TASK_GET, json!({"taskId":"1"})),
            tool_result(
                2,
                "get",
                TASK_GET,
                json!({"task":{
                    "id":"1","subject":"stale","description":"old",
                    "status":"pending","blocks":[],"blockedBy":[]
                }}),
            ),
            tool_result(
                3,
                "get",
                TASK_GET,
                json!({"task":{
                    "id":"1","subject":"current","description":"new",
                    "status":"in_progress","blocks":[],"blockedBy":[]
                }}),
            ),
        ];
        let snapshot = snapshot_from_projected_items(&items).expect("task snapshot");

        assert_eq!(snapshot.rows[0].subject, "current");
        assert_eq!(snapshot.rows[0].description.as_deref(), Some("new"));
        assert_eq!(snapshot.rows[0].status, TaskPanelStatus::InProgress);
        assert_eq!(snapshot.source_sequence, Some(3));
    }

    #[test]
    fn confirmed_deleted_update_removes_task() {
        let items = vec![
            create_use(1, "create", "subject"),
            create_result(2, "create", "1", "subject"),
            tool_use(
                3,
                "delete",
                TASK_UPDATE,
                json!({"taskId":"1","status":"deleted"}),
            ),
            tool_result(
                4,
                "delete",
                TASK_UPDATE,
                json!({
                    "success":true,
                    "taskId":"1",
                    "updatedFields":["deleted"],
                    "statusChange":{"from":"pending","to":"deleted"}
                }),
            ),
        ];
        assert!(
            snapshot_from_projected_items(&items)
                .expect("task snapshot")
                .is_empty()
        );
    }

    #[test]
    fn swarm_auto_owner_change_is_not_invented() {
        let items = vec![
            create_use(1, "create", "subject"),
            create_result(2, "create", "1", "subject"),
            tool_use(
                3,
                "update",
                TASK_UPDATE,
                json!({"taskId":"1","status":"in_progress"}),
            ),
            tool_result(
                4,
                "update",
                TASK_UPDATE,
                json!({
                    "success":true,
                    "taskId":"1",
                    "updatedFields":["owner","status"],
                    "statusChange":{"from":"pending","to":"in_progress"}
                }),
            ),
        ];
        let snapshot = snapshot_from_projected_items(&items).expect("task snapshot");

        assert_eq!(snapshot.rows[0].owner, None);
        assert_eq!(snapshot.rows[0].status, TaskPanelStatus::InProgress);
    }

    #[test]
    fn malformed_recognized_result_fails_closed_without_echoing_payload() {
        let error = snapshot_from_projected_items(&[
            create_use(1, "create", "exact subject"),
            create_result(2, "create", "1", "different subject"),
        ])
        .expect_err("mismatched successful result must fail closed");

        assert_eq!(error.sequence, 2);
        assert_eq!(error.code, "task_create_subject_mismatch");
        assert!(!error.to_string().contains("different subject"));
    }

    #[test]
    fn task_list_count_is_bounded() {
        let tasks = (0..=MAX_TASKS)
            .map(|index| {
                json!({
                    "id":index.to_string(),
                    "subject":format!("task {index}"),
                    "status":"pending",
                    "blockedBy":[]
                })
            })
            .collect::<Vec<_>>();
        let error = snapshot_from_projected_items(&[
            tool_use(1, "list", TASK_LIST, json!({})),
            tool_result(2, "list", TASK_LIST, json!({"tasks":tasks})),
        ])
        .expect_err("oversized task list must fail closed");

        assert_eq!(error.code, "task_list_task_limit_exceeded");
    }
}
