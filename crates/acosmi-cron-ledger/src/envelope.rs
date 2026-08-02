//! `ExecutionEnvelope` — the immutable, per-fire projection of a [`CronJob`].
//!
//! When a cron job fires, [`crate::LedgerConnection::fire_occurrence`] records
//! not just *that* it fired but *how it should run*: the session target, model,
//! thinking budget, timeout, permission posture, delivery config, payload
//! routing, and the message. That "how" is captured once, at fire time, into an
//! [`ExecutionEnvelope`] and stored as opaque JSON on the `occurrences` row
//! (`envelope_json`), with the routing target additionally denormalized into the
//! `target` column so a later claim/lease wave can index `(target, status,
//! scheduled_at_ms)` without parsing JSON.
//!
//! The envelope is a *projection*, not the job: it is a frozen copy of exactly
//! the fields a consumer needs to execute the turn, decoupled from any later
//! mutation of the owning `jobs` row. Because the occurrence insert is
//! `ON CONFLICT(job_id, scheduled_at_ms) DO NOTHING`, the first fire's envelope
//! is immutable — a crash-replay of the same firing never overwrites it.
//!
//! The mirror enums ([`EnvelopeTarget`], [`EnvelopeWakeMode`],
//! [`EnvelopePayloadKind`], [`EnvelopeDeliveryMode`]) are serde-compatible
//! copies of the `acosmi_scheduler` domain enums. They are mirrored (rather than
//! re-used) so the ledger's on-disk envelope format is owned by this crate and
//! cannot silently drift if a scheduler enum gains a variant — such a change
//! surfaces here as a non-exhaustive `match` compile error.

use acosmi_scheduler::{CronJob, DeliveryMode, PayloadKind, SessionTarget, WakeMode};
use serde::{Deserialize, Serialize};

/// Session routing target, mirroring [`acosmi_scheduler::SessionTarget`].
///
/// Serializes camelCase (`main` / `isolated` / `continuation`) to match the
/// scheduler's own wire form, and [`Self::as_str`] returns the identical string
/// stored in the `occurrences.target` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvelopeTarget {
    /// Write back into the main REPL session.
    Main,
    /// Run in an isolated, throwaway session.
    Isolated,
    /// Continue an existing chat session (see `session_key` / `channel_id` /
    /// `continuation_kind`).
    Continuation,
}

impl EnvelopeTarget {
    /// The stable string persisted into `occurrences.target`.
    ///
    /// Byte-identical to the camelCase serde form of the corresponding
    /// [`acosmi_scheduler::SessionTarget`] variant, so the denormalized column
    /// and the JSON envelope always agree.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Isolated => "isolated",
            Self::Continuation => "continuation",
        }
    }
}

impl From<&SessionTarget> for EnvelopeTarget {
    fn from(target: &SessionTarget) -> Self {
        match target {
            SessionTarget::Main => Self::Main,
            SessionTarget::Isolated => Self::Isolated,
            SessionTarget::Continuation => Self::Continuation,
        }
    }
}

/// Wake mode, mirroring [`acosmi_scheduler::WakeMode`] (kebab-case wire form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvelopeWakeMode {
    /// Deliver on the next heartbeat tick.
    NextHeartbeat,
    /// Wake immediately.
    Now,
}

impl From<&WakeMode> for EnvelopeWakeMode {
    fn from(wake_mode: &WakeMode) -> Self {
        match wake_mode {
            WakeMode::NextHeartbeat => Self::NextHeartbeat,
            WakeMode::Now => Self::Now,
        }
    }
}

/// Payload kind, mirroring [`acosmi_scheduler::PayloadKind`] (camelCase wire
/// form: `systemEvent` / `agentTurn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvelopePayloadKind {
    /// A system event (no agent turn).
    SystemEvent,
    /// An agent turn driven by `message`.
    AgentTurn,
}

impl From<&PayloadKind> for EnvelopePayloadKind {
    fn from(kind: &PayloadKind) -> Self {
        match kind {
            PayloadKind::SystemEvent => Self::SystemEvent,
            PayloadKind::AgentTurn => Self::AgentTurn,
        }
    }
}

/// Delivery mode, mirroring [`acosmi_scheduler::DeliveryMode`] (`none` /
/// `announce`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvelopeDeliveryMode {
    /// No delivery (result stays in-session).
    #[serde(rename = "none")]
    None,
    /// Announce the result over the configured channel.
    #[serde(rename = "announce")]
    Announce,
}

impl From<&DeliveryMode> for EnvelopeDeliveryMode {
    fn from(mode: &DeliveryMode) -> Self {
        match mode {
            DeliveryMode::None => Self::None,
            DeliveryMode::Announce => Self::Announce,
        }
    }
}

/// Delivery configuration captured for one fire.
///
/// A cron job carries delivery intent in two places: the structured
/// [`acosmi_scheduler::CronDelivery`] (`job.delivery`) and the flatter inline
/// hints on the payload (`job.payload.{deliver, channel, to,
/// best_effort_deliver}`). [`DeliveryEnvelope::from_job`] merges them into this
/// single `mode / channel / to / best_effort` shape with a documented
/// precedence: the structured `job.delivery` is authoritative when present and
/// the payload hints fill any field it leaves unset; with no structured
/// delivery, the payload hints are used directly (`deliver == Some(true)`
/// ⇒ [`EnvelopeDeliveryMode::Announce`], else [`EnvelopeDeliveryMode::None`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryEnvelope {
    /// Resolved delivery mode.
    pub mode: EnvelopeDeliveryMode,
    /// Destination channel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Destination address within the channel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Whether delivery is best-effort (failures do not fail the turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_effort: Option<bool>,
}

impl DeliveryEnvelope {
    /// Project a job's delivery intent, merging `job.delivery` over the inline
    /// `job.payload` delivery hints (see the type-level precedence note).
    fn from_job(job: &CronJob) -> Self {
        match job.delivery.as_ref() {
            Some(delivery) => Self {
                mode: EnvelopeDeliveryMode::from(&delivery.mode),
                channel: delivery
                    .channel
                    .clone()
                    .or_else(|| job.payload.channel.clone()),
                to: delivery.to.clone().or_else(|| job.payload.to.clone()),
                best_effort: delivery.best_effort.or(job.payload.best_effort_deliver),
            },
            None => Self {
                mode: if job.payload.deliver == Some(true) {
                    EnvelopeDeliveryMode::Announce
                } else {
                    EnvelopeDeliveryMode::None
                },
                channel: job.payload.channel.clone(),
                to: job.payload.to.clone(),
                best_effort: job.payload.best_effort_deliver,
            },
        }
    }
}

/// The immutable per-fire execution projection persisted on an occurrence.
///
/// Built by [`ExecutionEnvelope::from_job`] at the moment a job fires and stored
/// (as JSON) in `occurrences.envelope_json`. See the module docs for the
/// immutability and denormalization contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionEnvelope {
    /// Session routing target (`job.session_target`). Also denormalized into the
    /// `occurrences.target` column via [`EnvelopeTarget::as_str`].
    pub target: EnvelopeTarget,
    /// Model slug to run the turn with (`job.payload.model`), or `None` to let
    /// the consumer pick its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Thinking-effort selector (`job.payload.thinking`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Per-turn timeout in seconds (`job.payload.timeout_seconds`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<i32>,
    /// Permission posture for untrusted external content
    /// (`job.payload.allow_unsafe_external_content`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_allow_unsafe_external_content: Option<bool>,
    /// Merged delivery configuration (see [`DeliveryEnvelope`]).
    pub delivery: DeliveryEnvelope,
    /// Payload kind (`job.payload.kind`).
    pub payload_kind: EnvelopePayloadKind,
    /// Wake mode (`job.wake_mode`).
    pub wake_mode: EnvelopeWakeMode,
    /// Resolved prompt: `job.payload.message` if non-blank, else
    /// `job.payload.text` if non-blank (mirrors `OutboxEvent::from_job`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Continuation routing: original session key (`job.session_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Continuation routing: original channel id (`job.channel_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Continuation routing: `chat` | `notify` | `both` (`job.continuation_kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_kind: Option<String>,
}

impl ExecutionEnvelope {
    /// Project `job` into its immutable per-fire execution envelope.
    ///
    /// Pure and side-effect-free: every field is a clone/copy of a
    /// [`CronJob`] field taken at call time. `message` resolves
    /// `payload.message` (non-blank) or else `payload.text` (non-blank),
    /// matching `acosmi_scheduler::OutboxEvent::from_job`; `delivery` merges the
    /// structured and inline delivery sources (see [`DeliveryEnvelope`]).
    #[must_use]
    pub fn from_job(job: &CronJob) -> Self {
        let message = job
            .payload
            .message
            .clone()
            .filter(|message| !message.trim().is_empty())
            .or_else(|| {
                job.payload
                    .text
                    .clone()
                    .filter(|text| !text.trim().is_empty())
            });
        Self {
            target: EnvelopeTarget::from(&job.session_target),
            model: job.payload.model.clone(),
            thinking: job.payload.thinking.clone(),
            timeout_seconds: job.payload.timeout_seconds,
            permission_allow_unsafe_external_content: job.payload.allow_unsafe_external_content,
            delivery: DeliveryEnvelope::from_job(job),
            payload_kind: EnvelopePayloadKind::from(&job.payload.kind),
            wake_mode: EnvelopeWakeMode::from(&job.wake_mode),
            message,
            session_key: job.session_key.clone(),
            channel_id: job.channel_id.clone(),
            continuation_kind: job.continuation_kind.clone(),
        }
    }
}
