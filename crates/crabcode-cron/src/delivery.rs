//! Daemon-owned durable delivery lease state for the cron outbox.
//!
//! Every mutation is executed by `main.rs`'s single ledger actor.  The disk
//! format is one append-only journal per `(state identity, ledger epoch,
//! event id)`.  A complete newline-terminated record followed by `sync_all`
//! is the commit point.  A crash-torn final record is ignored and truncated
//! before the next append; a corrupt complete record fails loud.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acosmi_scheduler::OutboxEvent;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const DELIVERY_STORE_REL: &str = "cron/cron_delivery_v3";
const RETAINED_FLOOR_FILE: &str = "retained_floor.json";
const DELIVERY_STORE_VERSION: u8 = 1;
const RETAINED_FLOOR_VERSION: u8 = 1;
pub(crate) const DELIVERY_LEASE_MS: i64 = 15_000;
const MAX_CONSUMER_ID_BYTES: usize = 128;
/// Bound replay work and repeated full-payload lease records while preserving
/// unlimited logical retries through a durable generation checkpoint.
const JOURNAL_COMPACT_BYTES: usize = 256 * 1024;
const JOURNAL_COMPACT_TRANSITIONS: usize = 64;

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
type TokenMint = Arc<dyn Fn() -> String + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DeliveryConsumerKind {
    Tui,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeliveryConsumer {
    pub kind: DeliveryConsumerKind,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryLease {
    pub event_id: u64,
    pub generation: u64,
    pub lease_token: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryBeginOutcome {
    Acquired(DeliveryLease),
    Busy { event_id: u64, retry_at_ms: i64 },
    Accepted { event_id: u64, accepted_at_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryAcceptOutcome {
    Accepted {
        event_id: u64,
        generation: u64,
        accepted_at_ms: i64,
    },
    Fenced {
        event_id: u64,
        generation: u64,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryAbandonOutcome {
    Abandoned {
        event_id: u64,
        generation: u64,
    },
    Fenced {
        event_id: u64,
        generation: u64,
        reason: &'static str,
    },
    Absent {
        event_id: u64,
        generation: u64,
    },
}

#[derive(Debug, Error)]
pub(crate) enum DeliveryError {
    #[error("cron delivery state identity mismatch")]
    StateIdentityMismatch,
    #[error("cron delivery ledger epoch mismatch")]
    LedgerEpochMismatch,
    #[error("invalid cron delivery request: {0}")]
    InvalidRequest(String),
    #[error("cron delivery event identity collision for event {0}")]
    EventIdentityCollision(u64),
    #[error("cron delivery event {0} is not present in the retained outbox window")]
    EventNotRetained(u64),
    #[error("cron delivery lease generation exhausted for event {0}")]
    GenerationExhausted(u64),
    #[error("cron delivery event {event_id} is below durable retained floor {retained_floor}")]
    EventBelowRetainedFloor { event_id: u64, retained_floor: u64 },
    #[error("corrupt cron delivery journal: {0}")]
    CorruptState(String),
    #[error("cron outbox state unavailable: {0}")]
    OutboxStateUnavailable(String),
    #[error("cron delivery store unavailable: {0}")]
    Store(#[from] std::io::Error),
    #[error("cron delivery actor unavailable")]
    ActorUnavailable,
}

impl DeliveryError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::StateIdentityMismatch => "state_identity_mismatch",
            Self::LedgerEpochMismatch => "ledger_epoch_mismatch",
            Self::InvalidRequest(_) => "invalid_delivery_request",
            Self::EventIdentityCollision(_) => "event_identity_collision",
            Self::EventNotRetained(_) => "event_not_retained",
            Self::GenerationExhausted(_) => "generation_exhausted",
            Self::EventBelowRetainedFloor { .. } => "event_below_retained_floor",
            Self::CorruptState(_) => "delivery_state_corrupt",
            Self::OutboxStateUnavailable(_) => "outbox_state_unavailable",
            Self::Store(_) => "delivery_store_unavailable",
            Self::ActorUnavailable => "delivery_actor_unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transition", rename_all = "camelCase")]
enum DeliveryTransition {
    Checkpoint {
        version: u8,
        #[serde(rename = "stateIdentity")]
        state_identity: String,
        #[serde(rename = "ledgerEpoch")]
        ledger_epoch: String,
        event: OutboxEvent,
        #[serde(rename = "lastGeneration")]
        last_generation: u64,
    },
    LeaseGranted {
        version: u8,
        #[serde(rename = "stateIdentity")]
        state_identity: String,
        #[serde(rename = "ledgerEpoch")]
        ledger_epoch: String,
        event: OutboxEvent,
        generation: u64,
        #[serde(rename = "leaseToken")]
        lease_token: String,
        consumer: DeliveryConsumer,
        #[serde(rename = "leasedAtMs")]
        leased_at_ms: i64,
        #[serde(rename = "expiresAtMs")]
        expires_at_ms: i64,
    },
    Accepted {
        version: u8,
        #[serde(rename = "stateIdentity")]
        state_identity: String,
        #[serde(rename = "ledgerEpoch")]
        ledger_epoch: String,
        #[serde(rename = "eventId")]
        event_id: u64,
        generation: u64,
        #[serde(rename = "leaseToken")]
        lease_token: String,
        #[serde(rename = "acceptedAtMs")]
        accepted_at_ms: i64,
    },
    Abandoned {
        version: u8,
        #[serde(rename = "stateIdentity")]
        state_identity: String,
        #[serde(rename = "ledgerEpoch")]
        ledger_epoch: String,
        #[serde(rename = "eventId")]
        event_id: u64,
        generation: u64,
        #[serde(rename = "leaseToken")]
        lease_token: String,
        #[serde(rename = "abandonedAtMs")]
        abandoned_at_ms: i64,
    },
}

impl DeliveryTransition {
    fn version(&self) -> u8 {
        match self {
            Self::Checkpoint { version, .. }
            | Self::LeaseGranted { version, .. }
            | Self::Accepted { version, .. }
            | Self::Abandoned { version, .. } => *version,
        }
    }

    fn state_identity(&self) -> &str {
        match self {
            Self::Checkpoint { state_identity, .. }
            | Self::LeaseGranted { state_identity, .. }
            | Self::Accepted { state_identity, .. }
            | Self::Abandoned { state_identity, .. } => state_identity,
        }
    }

    fn ledger_epoch(&self) -> &str {
        match self {
            Self::Checkpoint { ledger_epoch, .. }
            | Self::LeaseGranted { ledger_epoch, .. }
            | Self::Accepted { ledger_epoch, .. }
            | Self::Abandoned { ledger_epoch, .. } => ledger_epoch,
        }
    }

    fn event_id(&self) -> u64 {
        match self {
            Self::Checkpoint { event, .. } | Self::LeaseGranted { event, .. } => event.event_id,
            Self::Accepted { event_id, .. } | Self::Abandoned { event_id, .. } => *event_id,
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveLease {
    generation: u64,
    lease_token: String,
    expires_at_ms: i64,
}

#[derive(Debug, Clone)]
struct AcceptedState {
    generation: u64,
    lease_token: String,
    accepted_at_ms: i64,
}

#[derive(Debug, Default)]
struct ReplayedState {
    event: Option<OutboxEvent>,
    generation: u64,
    active_lease: Option<ActiveLease>,
    accepted: Option<AcceptedState>,
    committed_bytes: usize,
    transition_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetainedFloorRecord {
    version: u8,
    ledger_epoch: String,
    retained_floor: u64,
}

pub(crate) struct DeliveryJournalStore {
    directory_anchor: PathBuf,
    state_identity: String,
    ledger_epoch: String,
    journal_dir: PathBuf,
    lease_ms: i64,
    now: Clock,
    mint_token: TokenMint,
    retained_floor: u64,
}

impl DeliveryJournalStore {
    pub(crate) async fn open(
        cron_dir: &Path,
        state_identity: String,
        ledger_epoch: String,
    ) -> Result<Self, DeliveryError> {
        let mut store = Self {
            directory_anchor: cron_dir.to_path_buf(),
            journal_dir: cron_dir.join(DELIVERY_STORE_REL).join(&ledger_epoch),
            state_identity,
            ledger_epoch,
            lease_ms: DELIVERY_LEASE_MS,
            now: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            mint_token: Arc::new(|| Uuid::new_v4().to_string()),
            retained_floor: 0,
        };
        store.initialize().await?;
        Ok(store)
    }

    #[cfg(test)]
    async fn open_for_test(
        cron_dir: &Path,
        state_identity: String,
        ledger_epoch: String,
        lease_ms: i64,
        now: Clock,
        mint_token: TokenMint,
    ) -> Result<Self, DeliveryError> {
        let mut store = Self::open(cron_dir, state_identity, ledger_epoch).await?;
        store.lease_ms = lease_ms;
        store.now = now;
        store.mint_token = mint_token;
        Ok(store)
    }

    async fn initialize(&mut self) -> Result<(), DeliveryError> {
        ensure_private_directory(&self.directory_anchor, &self.journal_dir).await?;
        self.cleanup_temps().await?;
        self.retained_floor = self.load_retained_floor().await?;
        self.cleanup_journals_below_floor().await?;
        self.cleanup_old_epoch_directories().await
    }

    async fn cleanup_temps(&self) -> Result<(), DeliveryError> {
        let mut entries = fs::read_dir(&self.journal_dir).await?;
        let mut removed_any = false;
        while let Some(entry) = entries.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let retained_floor_temp =
                name.starts_with(".retained-floor.") && name.ends_with(".tmp");
            let journal_compaction_temp =
                name.starts_with(".evt-") && name.ends_with(".compact.tmp");
            if retained_floor_temp || journal_compaction_temp {
                match fs::remove_file(entry.path()).await {
                    Ok(()) => removed_any = true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if removed_any {
            sync_directory(&self.journal_dir)?;
        }
        Ok(())
    }

    pub(crate) async fn begin(
        &self,
        expected_state_identity: &str,
        expected_ledger_epoch: &str,
        event: OutboxEvent,
        consumer: DeliveryConsumer,
    ) -> Result<DeliveryBeginOutcome, DeliveryError> {
        self.validate_namespace(expected_state_identity, expected_ledger_epoch)?;
        self.validate_event_and_consumer(&event, &consumer)?;
        self.validate_retained_floor(event.event_id)?;

        let state = self.load_state(event.event_id).await?;
        if let Some(canonical) = state.event.as_ref()
            && canonical != &event
        {
            return Err(DeliveryError::EventIdentityCollision(event.event_id));
        }
        if let Some(accepted) = state.accepted {
            return Ok(DeliveryBeginOutcome::Accepted {
                event_id: event.event_id,
                accepted_at_ms: accepted.accepted_at_ms,
            });
        }

        let observed_now_ms = (self.now)();
        if let Some(lease) = state.active_lease.as_ref()
            && lease.expires_at_ms > observed_now_ms
        {
            return Ok(DeliveryBeginOutcome::Busy {
                event_id: event.event_id,
                retry_at_ms: lease.expires_at_ms,
            });
        }

        // At this point the event is not accepted and any prior lease is
        // absent or expired. A checkpoint may safely replace replay history
        // before granting the next generation; a crash after replacement but
        // before the new lease preserves the last generation and canonical
        // event, so generation can never roll back.
        self.compact_for_next_lease_if_needed(event.event_id, &state)
            .await?;
        // Compaction can include multiple durable fsync/replace operations.
        // Refresh the clock afterwards so the newly returned lease always
        // receives its full lease interval instead of inheriting stale time.
        let grant_now_ms = (self.now)();

        let generation = state
            .generation
            .checked_add(1)
            .ok_or(DeliveryError::GenerationExhausted(event.event_id))?;
        let lease_token = (self.mint_token)();
        Uuid::parse_str(&lease_token).map_err(|error| {
            DeliveryError::InvalidRequest(format!("minted lease token is not a UUID: {error}"))
        })?;
        let expires_at_ms = grant_now_ms.checked_add(self.lease_ms).ok_or_else(|| {
            DeliveryError::InvalidRequest("lease expiry timestamp overflow".to_string())
        })?;
        let transition = DeliveryTransition::LeaseGranted {
            version: DELIVERY_STORE_VERSION,
            state_identity: self.state_identity.clone(),
            ledger_epoch: self.ledger_epoch.clone(),
            event: event.clone(),
            generation,
            lease_token: lease_token.clone(),
            consumer,
            leased_at_ms: grant_now_ms,
            expires_at_ms,
        };
        self.append_transition(event.event_id, &transition).await?;
        Ok(DeliveryBeginOutcome::Acquired(DeliveryLease {
            event_id: event.event_id,
            generation,
            lease_token,
            expires_at_ms,
        }))
    }

    pub(crate) async fn accept(
        &self,
        expected_state_identity: &str,
        expected_ledger_epoch: &str,
        event_id: u64,
        generation: u64,
        lease_token: &str,
    ) -> Result<DeliveryAcceptOutcome, DeliveryError> {
        self.validate_namespace(expected_state_identity, expected_ledger_epoch)?;
        self.validate_fence(event_id, generation, lease_token)?;
        self.validate_retained_floor(event_id)?;
        let state = self.load_state(event_id).await?;
        if let Some(accepted) = state.accepted {
            if accepted.generation == generation && accepted.lease_token == lease_token {
                return Ok(DeliveryAcceptOutcome::Accepted {
                    event_id,
                    generation,
                    accepted_at_ms: accepted.accepted_at_ms,
                });
            }
            return Ok(DeliveryAcceptOutcome::Fenced {
                event_id,
                generation,
                reason: "already_accepted_by_another_lease",
            });
        }
        let Some(lease) = state.active_lease else {
            return Ok(DeliveryAcceptOutcome::Fenced {
                event_id,
                generation,
                reason: "lease_absent",
            });
        };
        if lease.generation != generation || lease.lease_token != lease_token {
            return Ok(DeliveryAcceptOutcome::Fenced {
                event_id,
                generation,
                reason: "lease_replaced",
            });
        }
        let now_ms = (self.now)();
        if lease.expires_at_ms <= now_ms {
            return Ok(DeliveryAcceptOutcome::Fenced {
                event_id,
                generation,
                reason: "lease_expired",
            });
        }
        let transition = DeliveryTransition::Accepted {
            version: DELIVERY_STORE_VERSION,
            state_identity: self.state_identity.clone(),
            ledger_epoch: self.ledger_epoch.clone(),
            event_id,
            generation,
            lease_token: lease_token.to_string(),
            accepted_at_ms: now_ms,
        };
        self.append_transition(event_id, &transition).await?;
        Ok(DeliveryAcceptOutcome::Accepted {
            event_id,
            generation,
            accepted_at_ms: now_ms,
        })
    }

    pub(crate) async fn abandon(
        &self,
        expected_state_identity: &str,
        expected_ledger_epoch: &str,
        event_id: u64,
        generation: u64,
        lease_token: &str,
    ) -> Result<DeliveryAbandonOutcome, DeliveryError> {
        self.validate_namespace(expected_state_identity, expected_ledger_epoch)?;
        self.validate_fence(event_id, generation, lease_token)?;
        self.validate_retained_floor(event_id)?;
        let state = self.load_state(event_id).await?;
        if state.accepted.is_some() {
            return Ok(DeliveryAbandonOutcome::Fenced {
                event_id,
                generation,
                reason: "already_accepted",
            });
        }
        let Some(lease) = state.active_lease else {
            return Ok(DeliveryAbandonOutcome::Absent {
                event_id,
                generation,
            });
        };
        if lease.generation != generation || lease.lease_token != lease_token {
            return Ok(DeliveryAbandonOutcome::Fenced {
                event_id,
                generation,
                reason: "lease_replaced",
            });
        }
        let transition = DeliveryTransition::Abandoned {
            version: DELIVERY_STORE_VERSION,
            state_identity: self.state_identity.clone(),
            ledger_epoch: self.ledger_epoch.clone(),
            event_id,
            generation,
            lease_token: lease_token.to_string(),
            abandoned_at_ms: (self.now)(),
        };
        self.append_transition(event_id, &transition).await?;
        Ok(DeliveryAbandonOutcome::Abandoned {
            event_id,
            generation,
        })
    }

    /// Advance the monotonic retained floor.  The floor record is atomically
    /// replaced and durably committed *before* any journal is removed.  Once
    /// committed it is the permanent rejection proof for every event below
    /// the floor, so accepted, abandoned, expired, and live journals may all
    /// be deleted.  Startup retries cleanup after a crash between those steps.
    pub(crate) async fn advance_retained_floor(
        &mut self,
        oldest_retained_event_id: u64,
    ) -> Result<(), DeliveryError> {
        if oldest_retained_event_id == 0
            || oldest_retained_event_id > acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
        {
            return Err(DeliveryError::InvalidRequest(format!(
                "retained floor must be within 1..={}, got {oldest_retained_event_id}",
                acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
            )));
        }
        if oldest_retained_event_id <= self.retained_floor {
            return Ok(());
        }
        self.persist_retained_floor(oldest_retained_event_id)
            .await?;
        self.retained_floor = oldest_retained_event_id;
        self.cleanup_journals_below_floor().await
    }

    async fn load_retained_floor(&self) -> Result<u64, DeliveryError> {
        let path = self.retained_floor_path();
        let bytes = match read_regular_file_no_follow(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let record: RetainedFloorRecord = serde_json::from_slice(&bytes)
            .map_err(|error| DeliveryError::CorruptState(format!("{}: {error}", path.display())))?;
        if record.version != RETAINED_FLOOR_VERSION
            || record.ledger_epoch != self.ledger_epoch
            || record.retained_floor == 0
            || record.retained_floor > acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
        {
            return Err(DeliveryError::CorruptState(format!(
                "invalid retained floor record at {}",
                path.display()
            )));
        }
        Ok(record.retained_floor)
    }

    async fn persist_retained_floor(&self, retained_floor: u64) -> Result<(), DeliveryError> {
        ensure_private_directory(&self.directory_anchor, &self.journal_dir).await?;
        let path = self.retained_floor_path();
        let temp_path = self.journal_dir.join(format!(
            ".retained-floor.{}.{}.tmp",
            std::process::id(),
            Uuid::new_v4()
        ));
        let record = RetainedFloorRecord {
            version: RETAINED_FLOOR_VERSION,
            ledger_epoch: self.ledger_epoch.clone(),
            retained_floor,
        };
        let mut bytes = serde_json::to_vec(&record).map_err(|error| {
            DeliveryError::CorruptState(format!("serialize retained floor: {error}"))
        })?;
        bytes.push(b'\n');
        let result = async {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                options.mode(0o600);
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options.open(&temp_path).await?;
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            atomic_replace(&temp_path, &path).await?;
            sync_parent_directory(&path)
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
        }
        result.map_err(DeliveryError::from)
    }

    async fn cleanup_journals_below_floor(&self) -> Result<(), DeliveryError> {
        if self.retained_floor == 0 {
            return Ok(());
        }
        let mut entries = fs::read_dir(&self.journal_dir).await?;
        let mut removed_any = false;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                DeliveryError::CorruptState(format!(
                    "non-UTF8 delivery store entry in {}",
                    self.journal_dir.display()
                ))
            })?;
            if name == RETAINED_FLOOR_FILE || name.starts_with(".retained-floor.") {
                continue;
            }
            let Some(raw_id) = name
                .strip_prefix("evt-")
                .and_then(|value| value.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if !file_type.is_file() {
                return Err(DeliveryError::CorruptState(format!(
                    "delivery journal is not a regular file: {}",
                    entry.path().display()
                )));
            }
            let event_id = raw_id.parse::<u64>().map_err(|error| {
                DeliveryError::CorruptState(format!(
                    "invalid delivery journal name {}: {error}",
                    entry.path().display()
                ))
            })?;
            if event_id < self.retained_floor {
                match fs::remove_file(entry.path()).await {
                    Ok(()) => removed_any = true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if removed_any {
            sync_directory(&self.journal_dir)?;
        }
        Ok(())
    }

    async fn cleanup_old_epoch_directories(&self) -> Result<(), DeliveryError> {
        let Some(base_dir) = self.journal_dir.parent() else {
            return Err(DeliveryError::CorruptState(
                "delivery journal directory has no parent".to_string(),
            ));
        };
        let mut entries = fs::read_dir(base_dir).await?;
        let mut removed_any = false;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == self.ledger_epoch || Uuid::parse_str(name).is_err() {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).await?;
            if metadata_is_link_or_reparse(&metadata) {
                return Err(DeliveryError::CorruptState(format!(
                    "old delivery epoch entry is a symlink and was not followed: {}",
                    entry.path().display()
                )));
            }
            if metadata.is_dir() {
                fs::remove_dir_all(entry.path()).await?;
                removed_any = true;
            } else {
                return Err(DeliveryError::CorruptState(format!(
                    "old delivery epoch entry is not a directory: {}",
                    entry.path().display()
                )));
            }
        }
        if removed_any {
            sync_directory(base_dir)?;
        }
        Ok(())
    }

    fn validate_namespace(
        &self,
        expected_state_identity: &str,
        expected_ledger_epoch: &str,
    ) -> Result<(), DeliveryError> {
        if expected_state_identity != self.state_identity {
            return Err(DeliveryError::StateIdentityMismatch);
        }
        if expected_ledger_epoch != self.ledger_epoch {
            return Err(DeliveryError::LedgerEpochMismatch);
        }
        Ok(())
    }

    fn validate_event_and_consumer(
        &self,
        event: &OutboxEvent,
        consumer: &DeliveryConsumer,
    ) -> Result<(), DeliveryError> {
        if event.event_id == 0 || event.event_id > acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID {
            return Err(DeliveryError::InvalidRequest(format!(
                "eventId must be within 1..={}",
                acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
            )));
        }
        let id_len = consumer.id.len();
        if id_len == 0 || id_len > MAX_CONSUMER_ID_BYTES {
            return Err(DeliveryError::InvalidRequest(format!(
                "consumer.id must contain 1..={MAX_CONSUMER_ID_BYTES} bytes"
            )));
        }
        Ok(())
    }

    fn validate_fence(
        &self,
        event_id: u64,
        generation: u64,
        lease_token: &str,
    ) -> Result<(), DeliveryError> {
        if event_id == 0
            || event_id > acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
            || generation == 0
        {
            return Err(DeliveryError::InvalidRequest(format!(
                "eventId must be within 1..={} and generation must be greater than zero",
                acosmi_scheduler::outbox::MAX_SAFE_EVENT_ID
            )));
        }
        Uuid::parse_str(lease_token).map_err(|error| {
            DeliveryError::InvalidRequest(format!("leaseToken must be a UUID: {error}"))
        })?;
        Ok(())
    }

    fn validate_retained_floor(&self, event_id: u64) -> Result<(), DeliveryError> {
        if event_id < self.retained_floor {
            return Err(DeliveryError::EventBelowRetainedFloor {
                event_id,
                retained_floor: self.retained_floor,
            });
        }
        Ok(())
    }

    fn journal_path(&self, event_id: u64) -> PathBuf {
        self.journal_dir.join(format!("evt-{event_id}.jsonl"))
    }

    fn retained_floor_path(&self) -> PathBuf {
        self.journal_dir.join(RETAINED_FLOOR_FILE)
    }

    async fn load_state(&self, event_id: u64) -> Result<ReplayedState, DeliveryError> {
        let path = self.journal_path(event_id);
        let bytes = match read_regular_file_no_follow(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReplayedState::default());
            }
            Err(error) => return Err(error.into()),
        };
        let committed_len = committed_prefix_len(&bytes);
        let committed = &bytes[..committed_len];
        let mut state = ReplayedState {
            committed_bytes: committed_len,
            ..ReplayedState::default()
        };
        for (line_index, line) in committed.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            state.transition_count = state.transition_count.checked_add(1).ok_or_else(|| {
                DeliveryError::CorruptState(format!(
                    "transition count overflow for event {event_id}"
                ))
            })?;
            let transition: DeliveryTransition = serde_json::from_slice(line).map_err(|error| {
                DeliveryError::CorruptState(format!(
                    "{} line {}: {error}",
                    path.display(),
                    line_index + 1
                ))
            })?;
            self.apply_replayed_transition(event_id, transition, &mut state)?;
        }
        Ok(state)
    }

    fn apply_replayed_transition(
        &self,
        event_id: u64,
        transition: DeliveryTransition,
        state: &mut ReplayedState,
    ) -> Result<(), DeliveryError> {
        if transition.version() != DELIVERY_STORE_VERSION
            || transition.state_identity() != self.state_identity
            || transition.ledger_epoch() != self.ledger_epoch
            || transition.event_id() != event_id
        {
            return Err(DeliveryError::CorruptState(format!(
                "journal namespace/version mismatch for event {event_id}"
            )));
        }
        match transition {
            DeliveryTransition::Checkpoint {
                event,
                last_generation,
                ..
            } => {
                if state.event.is_some()
                    || state.generation != 0
                    || state.active_lease.is_some()
                    || state.accepted.is_some()
                    || last_generation == 0
                {
                    return Err(DeliveryError::CorruptState(format!(
                        "checkpoint must be the first valid transition for event {event_id}"
                    )));
                }
                state.event = Some(event);
                state.generation = last_generation;
            }
            DeliveryTransition::LeaseGranted {
                event,
                generation,
                lease_token,
                consumer: _,
                expires_at_ms,
                ..
            } => {
                if state.accepted.is_some() {
                    return Err(DeliveryError::CorruptState(format!(
                        "lease follows accepted transition for event {event_id}"
                    )));
                }
                if state.event.as_ref().is_some_and(|known| known != &event) {
                    return Err(DeliveryError::EventIdentityCollision(event_id));
                }
                let expected_generation = state
                    .generation
                    .checked_add(1)
                    .ok_or(DeliveryError::GenerationExhausted(event_id))?;
                if generation != expected_generation {
                    return Err(DeliveryError::CorruptState(format!(
                        "non-monotonic generation for event {event_id}: expected {expected_generation}, got {generation}"
                    )));
                }
                state.event.get_or_insert(event);
                state.generation = generation;
                state.active_lease = Some(ActiveLease {
                    generation,
                    lease_token,
                    expires_at_ms,
                });
            }
            DeliveryTransition::Accepted {
                generation,
                lease_token,
                accepted_at_ms,
                ..
            } => {
                let Some(lease) = state.active_lease.as_ref() else {
                    return Err(DeliveryError::CorruptState(format!(
                        "accepted transition has no active lease for event {event_id}"
                    )));
                };
                if lease.generation != generation || lease.lease_token != lease_token {
                    return Err(DeliveryError::CorruptState(format!(
                        "accepted transition fence mismatch for event {event_id}"
                    )));
                }
                state.accepted = Some(AcceptedState {
                    generation,
                    lease_token,
                    accepted_at_ms,
                });
                state.active_lease = None;
            }
            DeliveryTransition::Abandoned {
                generation,
                lease_token,
                ..
            } => {
                let Some(lease) = state.active_lease.as_ref() else {
                    return Err(DeliveryError::CorruptState(format!(
                        "abandon transition has no active lease for event {event_id}"
                    )));
                };
                if lease.generation != generation || lease.lease_token != lease_token {
                    return Err(DeliveryError::CorruptState(format!(
                        "abandon transition fence mismatch for event {event_id}"
                    )));
                }
                state.active_lease = None;
            }
        }
        Ok(())
    }

    async fn compact_for_next_lease_if_needed(
        &self,
        event_id: u64,
        state: &ReplayedState,
    ) -> Result<(), DeliveryError> {
        if state.generation == 0
            || (state.committed_bytes < JOURNAL_COMPACT_BYTES
                && state.transition_count < JOURNAL_COMPACT_TRANSITIONS)
        {
            return Ok(());
        }
        if state.accepted.is_some()
            || state
                .active_lease
                .as_ref()
                .is_some_and(|lease| lease.expires_at_ms > (self.now)())
        {
            return Err(DeliveryError::CorruptState(format!(
                "unsafe compaction attempt for active/accepted event {event_id}"
            )));
        }
        let event = state.event.clone().ok_or_else(|| {
            DeliveryError::CorruptState(format!(
                "cannot checkpoint event {event_id} without canonical payload"
            ))
        })?;
        let checkpoint = DeliveryTransition::Checkpoint {
            version: DELIVERY_STORE_VERSION,
            state_identity: self.state_identity.clone(),
            ledger_epoch: self.ledger_epoch.clone(),
            event,
            last_generation: state.generation,
        };
        self.replace_journal_with_checkpoint(event_id, &checkpoint)
            .await
    }

    async fn replace_journal_with_checkpoint(
        &self,
        event_id: u64,
        checkpoint: &DeliveryTransition,
    ) -> Result<(), DeliveryError> {
        ensure_private_directory(&self.directory_anchor, &self.journal_dir).await?;
        let path = self.journal_path(event_id);
        let temp_path = self.journal_dir.join(format!(
            ".evt-{event_id}.{}.{}.compact.tmp",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mut line = serde_json::to_vec(checkpoint).map_err(|error| {
            DeliveryError::CorruptState(format!("serialize delivery checkpoint: {error}"))
        })?;
        line.push(b'\n');
        let result = async {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                options.mode(0o600);
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = options.open(&temp_path).await?;
            file.write_all(&line).await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            atomic_replace(&temp_path, &path).await?;
            sync_parent_directory(&path)
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp_path).await;
        }
        result.map_err(DeliveryError::from)
    }

    async fn append_transition(
        &self,
        event_id: u64,
        transition: &DeliveryTransition,
    ) -> Result<(), DeliveryError> {
        ensure_private_directory(&self.directory_anchor, &self.journal_dir).await?;
        let path = self.journal_path(event_id);
        let existed = match fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
                return Err(DeliveryError::CorruptState(format!(
                    "delivery journal target is not a regular non-symlink file: {}",
                    path.display()
                )));
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let existing = match read_regular_file_no_follow(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let committed_len = committed_prefix_len(&existing);

        let mut options = OpenOptions::new();
        // Windows append-only handles cannot be truncated to discard a torn
        // tail. This actor is the single journal writer, so use a normal
        // read/write handle and seek explicitly before appending.
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).await?;
        if !file.metadata().await?.is_file() {
            return Err(DeliveryError::CorruptState(format!(
                "delivery journal handle is not a regular file: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        if committed_len < existing.len() {
            file.set_len(u64::try_from(committed_len).map_err(|_| {
                DeliveryError::CorruptState("journal length exceeds u64".to_string())
            })?)
            .await?;
            file.sync_all().await?;
        }
        file.seek(SeekFrom::End(0)).await?;
        let mut line = serde_json::to_vec(transition).map_err(|error| {
            DeliveryError::CorruptState(format!("serialize delivery transition: {error}"))
        })?;
        line.push(b'\n');
        file.write_all(&line).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        if !existed {
            sync_directory(&self.journal_dir)?;
        }
        Ok(())
    }
}

fn committed_prefix_len(bytes: &[u8]) -> usize {
    if bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    }
}

async fn ensure_private_directory(anchor: &Path, path: &Path) -> std::io::Result<()> {
    create_dir_all_durable(anchor, path).await?;
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            set_private_directory_permissions(parent).await?;
        }
        set_private_directory_permissions(path).await?;
    }
    Ok(())
}

#[cfg(unix)]
async fn set_private_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    if !file.metadata().await?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a directory handle: {}", path.display()),
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o700))
        .await?;
    file.sync_all().await
}

/// Create a directory chain and publish every new component durably on
/// POSIX. Windows has no dependable directory-flush API; file contents are
/// synced and metadata replacement uses `MoveFileExW(...WRITE_THROUGH)`.
fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    false
}

async fn create_dir_all_durable(anchor: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(anchor).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "delivery directory {} escapes anchor {}: {error}",
                path.display(),
                anchor.display()
            ),
        )
    })?;
    let anchor_metadata = fs::symlink_metadata(anchor).await?;
    if metadata_is_link_or_reparse(&anchor_metadata) || !anchor_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "delivery directory anchor is not a non-symlink directory: {}",
                anchor.display()
            ),
        ));
    }

    let mut current = anchor.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid delivery directory component in {}", path.display()),
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current).await {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "delivery path component is not a non-symlink directory: {}",
                            current.display()
                        ),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700)).await?;
                }
                if let Some(parent) = current.parent() {
                    sync_directory(parent)?;
                }
                sync_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn read_regular_file_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).await?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a regular non-symlink file: {}", path.display()),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("opened handle is not a regular file: {}", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Directory flushing is not a reliable Win32 contract. Atomic metadata
    // replacements below use MOVEFILE_WRITE_THROUGH; file data is sync_all'd.
    Ok(())
}

#[cfg(not(windows))]
async fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to).await?;
    sync_parent_directory(to)
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW is the Win32 atomic replace + write-through primitive.
async fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both buffers are NUL-terminated and live for the entire call.
    let result = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use acosmi_scheduler::{
        CronJob, CronJobState, CronPayload, CronSchedule, OutboxEventType, PayloadKind,
        ScheduleKind, SessionTarget, WakeMode,
    };

    use super::*;

    const STATE: &str = "cron-state-v1:test-state";
    const EPOCH: &str = "00000000-0000-4000-8000-000000000001";

    fn event(event_id: u64, message: &str) -> OutboxEvent {
        let job = CronJob {
            id: format!("job-{event_id}"),
            agent_id: None,
            name: format!("job-{event_id}"),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1,
            updated_at_ms: 1,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".to_string()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some(message.to_string()),
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        OutboxEvent::from_job(&job, event_id, OutboxEventType::Fired, 100)
    }

    fn consumer(id: &str) -> DeliveryConsumer {
        DeliveryConsumer {
            kind: DeliveryConsumerKind::Tui,
            id: id.to_string(),
        }
    }

    async fn test_store(dir: &Path, now: Arc<AtomicU64>) -> DeliveryJournalStore {
        let token_seq = Arc::new(AtomicU64::new(1));
        DeliveryJournalStore::open_for_test(
            dir,
            STATE.to_string(),
            EPOCH.to_string(),
            50,
            Arc::new(move || i64::try_from(now.load(Ordering::SeqCst)).unwrap()),
            Arc::new(move || {
                let n = token_seq.fetch_add(1, Ordering::SeqCst);
                format!("00000000-0000-4000-8000-{n:012}")
            }),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn lease_takeover_fences_old_generation_and_accept_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), Arc::clone(&now)).await;
        let event = event(7, "first");
        let first = store
            .begin(STATE, EPOCH, event.clone(), consumer("first"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(first) = first else {
            panic!("expected first lease")
        };
        assert!(matches!(
            store
                .begin(STATE, EPOCH, event.clone(), consumer("busy"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Busy { .. }
        ));

        now.store(1_051, Ordering::SeqCst);
        let second = store
            .begin(STATE, EPOCH, event.clone(), consumer("second"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(second) = second else {
            panic!("expected takeover lease")
        };
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert!(matches!(
            store
                .accept(
                    STATE,
                    EPOCH,
                    first.event_id,
                    first.generation,
                    &first.lease_token,
                )
                .await
                .unwrap(),
            DeliveryAcceptOutcome::Fenced { .. }
        ));
        assert!(matches!(
            store
                .abandon(
                    STATE,
                    EPOCH,
                    first.event_id,
                    first.generation,
                    &first.lease_token,
                )
                .await
                .unwrap(),
            DeliveryAbandonOutcome::Fenced { .. }
        ));
        let accepted = store
            .accept(
                STATE,
                EPOCH,
                second.event_id,
                second.generation,
                &second.lease_token,
            )
            .await
            .unwrap();
        assert!(matches!(accepted, DeliveryAcceptOutcome::Accepted { .. }));
        assert!(matches!(
            store
                .accept(
                    STATE,
                    EPOCH,
                    second.event_id,
                    second.generation,
                    &second.lease_token,
                )
                .await
                .unwrap(),
            DeliveryAcceptOutcome::Accepted { .. }
        ));

        let restarted = test_store(dir.path(), Arc::clone(&now)).await;
        assert!(matches!(
            restarted
                .begin(STATE, EPOCH, event, consumer("observer"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn abandon_allows_immediate_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), now).await;
        let event = event(8, "payload");
        let first = store
            .begin(STATE, EPOCH, event.clone(), consumer("first"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(first) = first else {
            panic!("expected lease")
        };
        assert!(matches!(
            store
                .abandon(
                    STATE,
                    EPOCH,
                    first.event_id,
                    first.generation,
                    &first.lease_token,
                )
                .await
                .unwrap(),
            DeliveryAbandonOutcome::Abandoned { .. }
        ));
        let second = store
            .begin(STATE, EPOCH, event, consumer("second"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(second) = second else {
            panic!("expected replacement lease")
        };
        assert_eq!(second.generation, 2);
    }

    #[tokio::test]
    async fn namespace_mismatch_and_event_collision_fail_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), now).await;
        assert!(matches!(
            store
                .begin("wrong", EPOCH, event(9, "payload"), consumer("a"))
                .await,
            Err(DeliveryError::StateIdentityMismatch)
        ));
        assert!(!store.journal_path(9).exists());

        let lease = store
            .begin(STATE, EPOCH, event(9, "payload"), consumer("a"))
            .await
            .unwrap();
        assert!(matches!(lease, DeliveryBeginOutcome::Acquired(_)));
        assert!(matches!(
            store
                .begin(STATE, EPOCH, event(9, "different"), consumer("b"))
                .await,
            Err(DeliveryError::EventIdentityCollision(9))
        ));
    }

    #[tokio::test]
    async fn crash_torn_tail_is_truncated_without_false_accept() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), now).await;
        let first = store
            .begin(STATE, EPOCH, event(10, "payload"), consumer("first"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(first) = first else {
            panic!("expected lease")
        };
        let path = store.journal_path(10);
        let mut raw = fs::read(&path).await.unwrap();
        raw.extend_from_slice(br#"{"transition":"accepted""#);
        fs::write(&path, raw).await.unwrap();

        assert!(matches!(
            store
                .abandon(
                    STATE,
                    EPOCH,
                    first.event_id,
                    first.generation,
                    &first.lease_token,
                )
                .await
                .unwrap(),
            DeliveryAbandonOutcome::Abandoned { .. }
        ));
        let repaired = fs::read(&path).await.unwrap();
        assert!(repaired.ends_with(b"\n"));
        for line in repaired.split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                serde_json::from_slice::<DeliveryTransition>(line).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn durable_floor_fences_stale_page_in_process_and_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let mut store = test_store(dir.path(), Arc::clone(&now)).await;
        let mut leases = Vec::new();
        for id in 1_u64..=3 {
            let begun = store
                .begin(STATE, EPOCH, event(id, "payload"), consumer("owner"))
                .await
                .unwrap();
            let DeliveryBeginOutcome::Acquired(lease) = begun else {
                panic!("expected lease")
            };
            leases.push(lease);
        }
        now.store(1_030, Ordering::SeqCst);
        let fourth = store
            .begin(STATE, EPOCH, event(4, "payload"), consumer("owner"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(fourth) = fourth else {
            panic!("expected live lease")
        };
        leases.push(fourth);
        store
            .accept(
                STATE,
                EPOCH,
                1,
                leases[0].generation,
                &leases[0].lease_token,
            )
            .await
            .unwrap();
        store
            .abandon(
                STATE,
                EPOCH,
                2,
                leases[1].generation,
                &leases[1].lease_token,
            )
            .await
            .unwrap();
        // ids 1/3 are expired; id=4 is deliberately still live. The durable floor
        // is the permanent fence, so neither journal must survive.
        now.store(1_051, Ordering::SeqCst);
        store.advance_retained_floor(5).await.unwrap();
        assert_eq!(store.retained_floor, 5);
        for id in 1_u64..=4 {
            assert!(!store.journal_path(id).exists());
            assert!(matches!(
                store
                    .begin(STATE, EPOCH, event(id, "payload"), consumer("stale"))
                    .await,
                Err(DeliveryError::EventBelowRetainedFloor {
                    event_id,
                    retained_floor: 5,
                }) if event_id == id
            ));
        }

        let restarted = test_store(dir.path(), Arc::clone(&now)).await;
        assert_eq!(restarted.retained_floor, 5);
        assert!(matches!(
            restarted
                .begin(STATE, EPOCH, event(1, "payload"), consumer("late"))
                .await,
            Err(DeliveryError::EventBelowRetainedFloor {
                event_id: 1,
                retained_floor: 5,
            })
        ));
        assert!(matches!(
            restarted
                .begin(STATE, EPOCH, event(5, "payload"), consumer("current"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Acquired(_)
        ));
    }

    #[tokio::test]
    async fn corrupt_complete_journal_line_fails_loud_but_torn_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), now).await;
        store
            .begin(STATE, EPOCH, event(20, "payload"), consumer("owner"))
            .await
            .unwrap();
        let path = store.journal_path(20);
        let mut bytes = fs::read(&path).await.unwrap();
        bytes.extend_from_slice(b"{not-json}\n");
        fs::write(&path, bytes).await.unwrap();
        assert!(matches!(
            store
                .begin(STATE, EPOCH, event(20, "payload"), consumer("again"))
                .await,
            Err(DeliveryError::CorruptState(_))
        ));

        let path = store.journal_path(21);
        fs::write(&path, b"{torn-no-newline").await.unwrap();
        assert!(matches!(
            store
                .begin(STATE, EPOCH, event(21, "payload"), consumer("repair"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Acquired(_)
        ));
        let repaired = fs::read(&path).await.unwrap();
        assert!(repaired.ends_with(b"\n"));
        assert_eq!(
            repaired
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn restart_finishes_cleanup_after_floor_commit_before_delete() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), Arc::clone(&now)).await;
        store
            .begin(STATE, EPOCH, event(1, "payload"), consumer("owner"))
            .await
            .unwrap();
        let journal = store.journal_path(1);
        store.persist_retained_floor(2).await.unwrap();
        assert!(journal.exists(), "simulate crash before deletion");
        drop(store);

        let restarted = test_store(dir.path(), now).await;
        assert_eq!(restarted.retained_floor, 2);
        assert!(
            !journal.exists(),
            "startup must finish floor-backed cleanup"
        );
    }

    #[tokio::test]
    async fn compaction_bounds_large_payload_abandons_and_preserves_accept_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), Arc::clone(&now)).await;
        let mut large_event = event(40, "payload");
        large_event.payload_message = Some("x".repeat(JOURNAL_COMPACT_BYTES + 4096));
        let canonical_size = serde_json::to_vec(&large_event).unwrap().len();

        for expected_generation in 1_u64..=8 {
            let begun = store
                .begin(STATE, EPOCH, large_event.clone(), consumer("retry"))
                .await
                .unwrap();
            let DeliveryBeginOutcome::Acquired(lease) = begun else {
                panic!("expected lease")
            };
            assert_eq!(lease.generation, expected_generation);
            store
                .abandon(STATE, EPOCH, 40, lease.generation, &lease.lease_token)
                .await
                .unwrap();
        }
        let journal = store.journal_path(40);
        let bounded_len = fs::metadata(&journal).await.unwrap().len() as usize;
        assert!(
            bounded_len < canonical_size * 3,
            "checkpoint must bound repeated full-payload records: {bounded_len} vs {canonical_size}"
        );
        drop(store);

        let restarted = test_store(dir.path(), Arc::clone(&now)).await;
        let ninth = restarted
            .begin(STATE, EPOCH, large_event.clone(), consumer("restart"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(ninth) = ninth else {
            panic!("expected ninth lease")
        };
        assert_eq!(ninth.generation, 9, "generation must not roll back");
        restarted
            .accept(STATE, EPOCH, 40, ninth.generation, &ninth.lease_token)
            .await
            .unwrap();
        drop(restarted);

        let accepted_restart = test_store(dir.path(), now).await;
        assert!(matches!(
            accepted_restart
                .begin(STATE, EPOCH, large_event, consumer("observer"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn checkpoint_crash_boundaries_and_expired_retries_keep_generation_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), Arc::clone(&now)).await;
        let retained = event(41, "small");

        for expected_generation in 1_u64..=32 {
            let begun = store
                .begin(STATE, EPOCH, retained.clone(), consumer("abandon"))
                .await
                .unwrap();
            let DeliveryBeginOutcome::Acquired(lease) = begun else {
                panic!("expected lease")
            };
            assert_eq!(lease.generation, expected_generation);
            store
                .abandon(STATE, EPOCH, 41, lease.generation, &lease.lease_token)
                .await
                .unwrap();
        }
        let replayed = store.load_state(41).await.unwrap();
        assert_eq!(replayed.transition_count, JOURNAL_COMPACT_TRANSITIONS);
        store
            .compact_for_next_lease_if_needed(41, &replayed)
            .await
            .unwrap();
        let compacted = fs::read(store.journal_path(41)).await.unwrap();
        assert_eq!(
            compacted
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            1,
            "crash after checkpoint replacement must leave one durable snapshot"
        );

        let stale_temp = store.journal_dir.join(".evt-41.crash.compact.tmp");
        fs::write(&stale_temp, b"uncommitted checkpoint")
            .await
            .unwrap();
        drop(store);
        let restarted = test_store(dir.path(), Arc::clone(&now)).await;
        assert!(!stale_temp.exists(), "startup must remove pre-rename temp");

        let mut last_generation = 32;
        for _ in 0..35 {
            let begun = restarted
                .begin(STATE, EPOCH, retained.clone(), consumer("expired"))
                .await
                .unwrap();
            let DeliveryBeginOutcome::Acquired(lease) = begun else {
                panic!("expected expired-lease takeover")
            };
            last_generation += 1;
            assert_eq!(lease.generation, last_generation);
            now.fetch_add(51, Ordering::SeqCst);
        }
        let replayed = restarted.load_state(41).await.unwrap();
        assert_eq!(replayed.generation, 67);
        assert!(
            replayed.transition_count < JOURNAL_COMPACT_TRANSITIONS,
            "expired retry journal must remain compacted"
        );
    }

    #[tokio::test]
    async fn new_lease_clock_is_refreshed_after_durable_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let initial_now = Arc::new(AtomicU64::new(1_000));
        let mut store = test_store(dir.path(), initial_now).await;
        let mut large_event = event(42, "payload");
        large_event.payload_message = Some("x".repeat(JOURNAL_COMPACT_BYTES + 1));
        let first = store
            .begin(STATE, EPOCH, large_event.clone(), consumer("first"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(first) = first else {
            panic!("expected first lease")
        };
        store
            .abandon(STATE, EPOCH, 42, first.generation, &first.lease_token)
            .await
            .unwrap();

        let clock_calls = Arc::new(AtomicU64::new(0));
        store.now = {
            let clock_calls = Arc::clone(&clock_calls);
            Arc::new(move || match clock_calls.fetch_add(1, Ordering::SeqCst) {
                0 => 10_000,
                _ => 20_000,
            })
        };
        let second = store
            .begin(STATE, EPOCH, large_event, consumer("second"))
            .await
            .unwrap();
        let DeliveryBeginOutcome::Acquired(second) = second else {
            panic!("expected second lease")
        };
        assert_eq!(second.generation, 2);
        assert_eq!(
            second.expires_at_ms, 20_050,
            "lease interval must start after compaction, not before it"
        );
        assert!(clock_calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn opening_new_epoch_removes_old_epoch_state_and_keeps_namespaces_isolated() {
        const NEXT_EPOCH: &str = "00000000-0000-4000-8000-000000000099";
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let old = test_store(dir.path(), Arc::clone(&now)).await;
        old.begin(STATE, EPOCH, event(1, "old"), consumer("old"))
            .await
            .unwrap();
        let old_dir = old.journal_dir.clone();
        drop(old);

        let fresh = DeliveryJournalStore::open_for_test(
            dir.path(),
            STATE.to_string(),
            NEXT_EPOCH.to_string(),
            50,
            Arc::new(move || i64::try_from(now.load(Ordering::SeqCst)).unwrap()),
            Arc::new(|| "00000000-0000-4000-8000-000000000777".to_string()),
        )
        .await
        .unwrap();
        assert!(
            !old_dir.exists(),
            "old epoch directory must be bounded away"
        );
        assert!(matches!(
            fresh
                .begin(STATE, EPOCH, event(1, "old"), consumer("stale"))
                .await,
            Err(DeliveryError::LedgerEpochMismatch)
        ));
        assert!(matches!(
            fresh
                .begin(STATE, NEXT_EPOCH, event(1, "new"), consumer("fresh"))
                .await
                .unwrap(),
            DeliveryBeginOutcome::Acquired(_)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delivery_store_rejects_symlink_epoch_and_journal_targets() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = dir.path().join(DELIVERY_STORE_REL);
        fs::create_dir_all(&base).await.unwrap();
        symlink(outside.path(), base.join(EPOCH)).unwrap();
        assert!(matches!(
            DeliveryJournalStore::open(dir.path(), STATE.to_string(), EPOCH.to_string()).await,
            Err(DeliveryError::Store(_))
        ));
        assert!(
            fs::read_dir(outside.path())
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );

        fs::remove_file(base.join(EPOCH)).await.unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let store = test_store(dir.path(), now).await;
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, b"do-not-touch").await.unwrap();
        symlink(&outside_file, store.journal_path(30)).unwrap();
        assert!(matches!(
            store
                .begin(STATE, EPOCH, event(30, "payload"), consumer("owner"))
                .await,
            Err(DeliveryError::Store(_))
        ));
        assert_eq!(fs::read(&outside_file).await.unwrap(), b"do-not-touch");
    }
}
