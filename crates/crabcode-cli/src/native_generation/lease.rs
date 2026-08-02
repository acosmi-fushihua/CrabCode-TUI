//! Cross-language native-generation lifetime lease.
//!
//! The stable/native launcher and TypeScript updater share one lock domain:
//! `locks/<version>.lock` is the short per-version transaction record and
//! `locks/<version>.process.<pid>.lock` is a process-lifetime record. Native
//! registration uses the same versioned owner-record bakery as `pidLock.ts`, so
//! GC cannot linearize deletion between generation selection and this lease
//! becoming visible. Guard ownership never depends on wall time or a shared
//! pathname that an old owner could unlink after replacement.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use acosmi_supervisor::topology::{ProcessId, SupervisorConfig};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const LEASE_PROTOCOL_ENV: &str = "CRABCODE_NATIVE_GENERATION_LEASE_PROTOCOL";
pub(super) const LEASE_PATH_ENV: &str = "CRABCODE_NATIVE_GENERATION_LEASE_PATH";
pub(super) const LEASE_OWNER_PID_ENV: &str = "CRABCODE_NATIVE_GENERATION_LEASE_OWNER_PID";
pub(super) const LEASE_OWNER_TOKEN_ENV: &str = "CRABCODE_NATIVE_GENERATION_LEASE_OWNER_TOKEN";
pub(super) const LEASE_PROCESS_START_IDENTITY_ENV: &str =
    "CRABCODE_NATIVE_GENERATION_LEASE_PROCESS_START_IDENTITY";
pub(super) const LEASE_GENERATION_PATH_ENV: &str =
    "CRABCODE_NATIVE_GENERATION_LEASE_GENERATION_PATH";

const LEASE_PROTOCOL_VERSION: &str = "1";
const GENERATION_LEASE_KIND: &str = "native-generation-handoff-v1";
const ACQUIRE_RETRY_DELAY: Duration = Duration::from_millis(50);
const ACQUIRE_RETRY_COUNT: usize = 120;
const LEGACY_LOCK_MIGRATION_MIN_AGE: Duration = Duration::from_secs(60);
const MAX_LOCK_RECORD_BYTES: u64 = 64 * 1024;
const ACQUIRE_GUARD_PROTOCOL: &str = "crabcode-acquire-guard-owner-v2";
const ACQUIRE_GUARD_RECORDS_DIR: &str = ACQUIRE_GUARD_PROTOCOL;
const MAX_ACQUIRE_GUARD_RECORD_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PidLockRecord {
    pid: u32,
    version: String,
    exec_path: String,
    acquired_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation_path: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AcquireGuardPhase {
    Choosing,
    Owner,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcquireGuardRecord {
    protocol: String,
    phase: AcquireGuardPhase,
    pid: u32,
    process_start_identity: String,
    owner_token: String,
    /// Unsigned decimal u64. Choosing records carry JSON null.
    ticket: Option<String>,
}

#[derive(Clone, Debug)]
struct ParsedAcquireGuardRecord {
    path: PathBuf,
    raw: Vec<u8>,
    record: AcquireGuardRecord,
    ticket: Option<u64>,
}

#[derive(Clone, Debug)]
struct OwnerIdentity {
    pid: u32,
    token: String,
    process_start_identity: String,
    exec_path: String,
}

impl OwnerIdentity {
    fn current_with_token(token: Option<String>) -> Result<Self> {
        let pid = std::process::id();
        let process_start_identity = match observe_process(pid) {
            ProcessObservation::AliveWithIdentity(identity) => identity,
            ProcessObservation::AliveUnknown => {
                bail!("cannot acquire native generation lease without a process birth identity")
            }
            ProcessObservation::Dead => {
                bail!("current native launcher process is unexpectedly not observable")
            }
        };
        let exec_path = protocol_path_string(
            &std::env::current_exe().context("cannot resolve native lease owner executable")?,
        )?;
        Ok(Self {
            pid,
            token: token.unwrap_or_else(|| Uuid::new_v4().to_string()),
            process_start_identity,
            exec_path,
        })
    }
}

#[derive(Clone, Debug)]
struct LeaseHandoff {
    lease_path: PathBuf,
    owner_pid: u32,
    owner_token: String,
    process_start_identity: String,
    generation_path: String,
}

impl LeaseHandoff {
    fn from_environment() -> Result<Option<Self>> {
        let names = [
            LEASE_PROTOCOL_ENV,
            LEASE_PATH_ENV,
            LEASE_OWNER_PID_ENV,
            LEASE_OWNER_TOKEN_ENV,
            LEASE_PROCESS_START_IDENTITY_ENV,
            LEASE_GENERATION_PATH_ENV,
        ];
        let values = names.map(std::env::var_os);
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        if values.iter().any(Option::is_none) {
            bail!("native generation lease handoff environment is incomplete");
        }
        let protocol = os_string(values[0].clone(), LEASE_PROTOCOL_ENV)?;
        if protocol != LEASE_PROTOCOL_VERSION {
            bail!("unsupported native generation lease protocol {protocol}");
        }
        let lease_path = PathBuf::from(values[1].clone().expect("checked above"));
        if !lease_path.is_absolute() {
            bail!("native generation lease handoff path must be absolute");
        }
        let owner_pid = os_string(values[2].clone(), LEASE_OWNER_PID_ENV)?
            .parse::<u32>()
            .context("native generation lease owner PID is invalid")?;
        let owner_token = os_string(values[3].clone(), LEASE_OWNER_TOKEN_ENV)?;
        let process_start_identity =
            os_string(values[4].clone(), LEASE_PROCESS_START_IDENTITY_ENV)?;
        let generation_path = os_string(values[5].clone(), LEASE_GENERATION_PATH_ENV)?;
        Ok(Some(Self {
            lease_path,
            owner_pid,
            owner_token,
            process_start_identity,
            generation_path,
        }))
    }

    fn matches(
        &self,
        expected_path: &Path,
        version: &str,
        generation_path: &str,
        owner: &OwnerIdentity,
        record: &PidLockRecord,
    ) -> bool {
        self.lease_path == expected_path
            && self.owner_pid == owner.pid
            && self.owner_token == owner.token
            && self.process_start_identity == owner.process_start_identity
            && self.generation_path == generation_path
            && record.pid == owner.pid
            && record.version == version
            && record.owner_token.as_deref() == Some(owner.token.as_str())
            && record.process_start_identity.as_deref()
                == Some(owner.process_start_identity.as_str())
            && record.lease_kind.as_deref() == Some(GENERATION_LEASE_KIND)
            && record.generation_path.as_deref() == Some(generation_path)
    }
}

fn os_string(value: Option<OsString>, label: &str) -> Result<String> {
    value
        .expect("lease environment presence checked")
        .into_string()
        .map_err(|_| anyhow::anyhow!("{label} is not valid Unicode"))
}

/// A PID/birth-identity record that protects one immutable generation.
///
/// The lifetime record is deliberately not unlinked by Drop: Rust teardown
/// still runs while the OS reports this PID alive. GC proves the PID/birth
/// identity dead and then retires the stale record under the acquisition guard,
/// so power loss needs no time-based seven-day lease and shutdown has no gap.
pub(crate) struct GenerationLease {
    path: PathBuf,
    record: PidLockRecord,
}

impl GenerationLease {
    /// Stable-launcher acquisition. Inherited shell values are ignored; this
    /// process creates a fresh capability before generation spawn/exec.
    pub(super) fn acquire_fresh<F>(
        generation: &Path,
        locks_dir: &Path,
        validate_generation: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<()>,
    {
        Self::acquire_inner(generation, locks_dir, None, validate_generation)
    }

    /// Generation acquisition. A POSIX stable-launcher exec preserves PID and
    /// birth identity, so the new image adopts only an exact capability match.
    /// Windows has a new PID and therefore creates a second native record while
    /// the stable parent continues protecting the pre-spawn boundary.
    pub(super) fn acquire_or_adopt<F>(
        generation: &Path,
        locks_dir: &Path,
        validate_generation: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<()>,
    {
        let handoff = LeaseHandoff::from_environment()?;
        Self::acquire_inner(generation, locks_dir, handoff, validate_generation)
    }

    fn acquire_inner<F>(
        generation: &Path,
        locks_dir: &Path,
        handoff: Option<LeaseHandoff>,
        validate_generation: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<()>,
    {
        let generation = fs::canonicalize(generation).with_context(|| {
            format!(
                "cannot canonicalize native lease generation {}",
                generation.display()
            )
        })?;
        let version = generation
            .file_name()
            .and_then(|value| value.to_str())
            .context("native lease generation has no Unicode version name")?
            .to_owned();
        let generation_path = protocol_path_string(&generation)?;
        let locks_dir = prepare_locks_directory(locks_dir)?;
        let mut owner = OwnerIdentity::current_with_token(None)?;
        let handoff = match handoff {
            Some(hint) if hint.owner_pid == owner.pid => {
                if hint.process_start_identity != owner.process_start_identity {
                    bail!("native generation lease handoff birth identity does not match owner");
                }
                owner.token.clone_from(&hint.owner_token);
                Some(hint)
            }
            // Windows stable launchers have a different parent PID. Their
            // record covers the pre-spawn interval; this generation now mints
            // its own record and later hands that child-bound tuple to Bun.
            Some(_) | None => None,
        };
        let lease_path = locks_dir.join(format!("{version}.process.{}.lock", owner.pid));

        // This short transaction is the linearization point shared with GC.
        // Validation happens after acquisition so a GC that won first cannot
        // leave us executing from a deleted/incomplete generation.
        let _transaction =
            VersionTransaction::acquire(&locks_dir, &version, &generation_path, &owner)?;
        validate_generation()?;

        if let Some(existing) = read_regular_record_if_present(&lease_path)? {
            let can_adopt = handoff.as_ref().is_some_and(|hint| {
                hint.matches(&lease_path, &version, &generation_path, &owner, &existing)
            });
            if can_adopt && record_is_active(&existing) {
                return Ok(Self {
                    path: lease_path,
                    record: existing,
                });
            }
            if record_is_active(&existing) {
                bail!(
                    "native generation lease path is already owned by a live process: {}",
                    lease_path.display()
                );
            }
            remove_regular_file(&lease_path)?;
        } else if handoff.is_some() {
            bail!("native generation lease handoff record is missing");
        } else if regular_file_entry_exists(&lease_path)? {
            remove_regular_file(&lease_path)?;
        }

        let record = PidLockRecord {
            pid: owner.pid,
            version,
            exec_path: owner.exec_path.clone(),
            acquired_at: unix_time_millis()?,
            owner_token: Some(owner.token.clone()),
            process_start_identity: Some(owner.process_start_identity.clone()),
            lease_kind: Some(GENERATION_LEASE_KIND.to_owned()),
            generation_path: Some(generation_path),
        };
        write_record_exclusive(&lease_path, &record)?;
        let verified = read_regular_record(&lease_path)?;
        if verified != record {
            remove_if_owned(&lease_path, &record);
            bail!("native generation lease durable owner verification failed");
        }
        Ok(Self {
            path: lease_path,
            record,
        })
    }

    pub(crate) fn inject_command_environment(&self, command: &mut Command) -> Result<()> {
        for (name, value) in self.environment()? {
            command.env(name, value);
        }
        Ok(())
    }

    pub(crate) fn inject_ts_environment(&self, config: &mut SupervisorConfig) -> Result<()> {
        let ts = config
            .processes
            .iter_mut()
            .find(|process| process.id == ProcessId::ts_session())
            .context("supervisor configuration has no TypeScript session process")?;
        // Explicit insertion wins over inherited shell/settings values. Only
        // the native owner record can mint this capability tuple.
        for (name, value) in self.environment()? {
            let _ = ts.env.insert(name.to_owned(), value);
        }
        Ok(())
    }

    fn environment(&self) -> Result<Vec<(&'static str, String)>> {
        let owner_token = self
            .record
            .owner_token
            .clone()
            .context("native generation lease has no owner token")?;
        let process_start_identity = self
            .record
            .process_start_identity
            .clone()
            .context("native generation lease has no process birth identity")?;
        let generation_path = self
            .record
            .generation_path
            .clone()
            .context("native generation lease has no generation path")?;
        Ok(vec![
            (LEASE_PROTOCOL_ENV, LEASE_PROTOCOL_VERSION.to_owned()),
            (LEASE_PATH_ENV, protocol_path_string(&self.path)?),
            (LEASE_OWNER_PID_ENV, self.record.pid.to_string()),
            (LEASE_OWNER_TOKEN_ENV, owner_token),
            (LEASE_PROCESS_START_IDENTITY_ENV, process_start_identity),
            (LEASE_GENERATION_PATH_ENV, generation_path),
        ])
    }
}

struct VersionTransaction {
    path: PathBuf,
    record: PidLockRecord,
}

impl VersionTransaction {
    fn acquire(
        locks_dir: &Path,
        version: &str,
        generation_path: &str,
        owner: &OwnerIdentity,
    ) -> Result<Self> {
        let path = locks_dir.join(format!("{version}.lock"));
        let guard_path = suffixed_path(&path, ".acquire-guard");
        for _ in 0..ACQUIRE_RETRY_COUNT {
            let Some(guard) = AcquisitionGuard::try_acquire(&guard_path, owner)? else {
                thread::sleep(ACQUIRE_RETRY_DELAY);
                continue;
            };

            // Pre-PID-lock releases used the lock path itself as an empty
            // proper-lockfile directory. Bun cannot migrate it because this
            // native transaction runs before Bun exists, so perform the same
            // conservative migration here: age/emptiness are necessary but
            // never sufficient without two quiescent process scans.
            retire_legacy_lock_directory(&path, owner.pid)?;

            if let Some(existing) = read_regular_record_if_present(&path)? {
                if record_is_active(&existing) {
                    drop(guard);
                    thread::sleep(ACQUIRE_RETRY_DELAY);
                    continue;
                }
                remove_regular_file(&path)?;
            } else if regular_file_entry_exists(&path)? {
                remove_regular_file(&path)?;
            }

            let record = PidLockRecord {
                pid: owner.pid,
                version: version.to_owned(),
                exec_path: owner.exec_path.clone(),
                acquired_at: unix_time_millis()?,
                owner_token: Some(owner.token.clone()),
                process_start_identity: Some(owner.process_start_identity.clone()),
                lease_kind: None,
                generation_path: Some(generation_path.to_owned()),
            };
            match write_record_exclusive(&path, &record) {
                Ok(()) => {
                    let verified = read_regular_record(&path)?;
                    if verified != record {
                        remove_if_owned(&path, &record);
                        bail!("native version transaction owner verification failed");
                    }
                    return Ok(Self { path, record });
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == ErrorKind::AlreadyExists) =>
                {
                    // A non-cooperating writer appeared despite the guard.
                    // Never unlink an unverified replacement; retry/fail closed.
                }
                Err(error) => return Err(error),
            }
            drop(guard);
            thread::sleep(ACQUIRE_RETRY_DELAY);
        }
        bail!("could not acquire native generation version transaction")
    }
}

impl Drop for VersionTransaction {
    fn drop(&mut self) {
        remove_if_owned(&self.path, &self.record);
    }
}

#[derive(Debug)]
struct AcquisitionGuard {
    owner: ParsedAcquireGuardRecord,
    retiring_token: String,
}

impl AcquisitionGuard {
    /// Acquire the cross-language owner-record bakery.
    ///
    /// Linearization is the end of the post-publication scan: every earlier
    /// live chooser has published, this owner is the minimum live
    /// `(ticket, token)` tuple, and a later chooser must observe this ticket
    /// before choosing its own.
    fn try_acquire(path: &Path, process_owner: &OwnerIdentity) -> Result<Option<Self>> {
        let Some(records_dir) = initialize_acquire_guard_records_dir(path, process_owner.pid)?
        else {
            return Ok(None);
        };
        let retiring_token = Uuid::new_v4().to_string();
        let choosing = AcquireGuardRecord {
            protocol: ACQUIRE_GUARD_PROTOCOL.to_owned(),
            phase: AcquireGuardPhase::Choosing,
            pid: process_owner.pid,
            process_start_identity: process_owner.process_start_identity.clone(),
            owner_token: retiring_token.clone(),
            ticket: None,
        };
        let choosing_path = records_dir.join(guard_record_name(&choosing));
        write_guard_record_exclusive(&choosing_path, &choosing)?;

        let mut owner: Option<ParsedAcquireGuardRecord> = None;
        let result = (|| -> Result<Option<Self>> {
            let before_publication = scan_active_guard_records(&records_dir, &retiring_token)?;
            let max_ticket = before_publication
                .iter()
                .filter_map(|candidate| candidate.ticket)
                .max()
                .unwrap_or(0);
            let ticket = max_ticket
                .checked_add(1)
                .context("native acquisition guard ticket space is exhausted")?;
            let owner_record = AcquireGuardRecord {
                phase: AcquireGuardPhase::Owner,
                ticket: Some(ticket.to_string()),
                ..choosing.clone()
            };
            let owner_path = records_dir.join(guard_record_name(&owner_record));
            write_guard_record_exclusive(&owner_path, &owner_record)?;
            let published_owner =
                read_guard_record(&owner_path, &guard_record_name(&owner_record))?;
            owner = Some(published_owner.clone());

            let observed_choosing =
                read_guard_record(&choosing_path, &guard_record_name(&choosing))?;
            retire_observed_guard_record(&observed_choosing, &retiring_token)?;

            let after_publication = scan_active_guard_records(&records_dir, &retiring_token)?;
            let blocked = after_publication.iter().any(|candidate| {
                candidate.record.owner_token != retiring_token
                    && (candidate.record.phase == AcquireGuardPhase::Choosing
                        || owner_tuple_precedes(candidate, ticket, &retiring_token))
            });
            if blocked {
                return Ok(None);
            }

            Ok(Some(Self {
                owner: published_owner,
                retiring_token: retiring_token.clone(),
            }))
        })();

        if !matches!(result, Ok(Some(_))) {
            retire_guard_record_if_exact(&choosing_path, &choosing, &retiring_token);
            if let Some(owner) = owner.as_ref() {
                let _ = retire_observed_guard_record(owner, &retiring_token);
            }
        }
        result
    }
}

impl Drop for AcquisitionGuard {
    fn drop(&mut self) {
        let _ = retire_observed_guard_record(&self.owner, &self.retiring_token);
    }
}

fn initialize_acquire_guard_records_dir(
    guard_path: &Path,
    owner_pid: u32,
) -> Result<Option<PathBuf>> {
    let records_dir = guard_path.join(ACQUIRE_GUARD_RECORDS_DIR);
    let created_root = match fs::create_dir(guard_path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot create version acquisition guard root {}",
                    guard_path.display()
                )
            });
        }
    };
    assert_real_directory(guard_path, "version acquisition guard root")?;

    if created_root {
        // Atomic mkdir is the permanent protocol marker. A crash between the
        // outer and inner mkdir leaves an ownerless legacy-ambiguous directory;
        // it is initialized only after the conservative proof below.
        fs::create_dir(&records_dir).with_context(|| {
            format!(
                "cannot create version acquisition guard protocol directory {}",
                records_dir.display()
            )
        })?;
    } else if !real_directory_exists(&records_dir)? {
        let before = fs::symlink_metadata(guard_path).with_context(|| {
            format!(
                "cannot inspect ownerless acquisition guard {}",
                guard_path.display()
            )
        })?;
        let before_identity = directory_snapshot_identity(guard_path, &before);
        let age = match SystemTime::now().duration_since(
            before
                .modified()
                .context("ownerless acquisition guard has no modification time")?,
        ) {
            Ok(age) => age,
            // Clock rollback/future mtime is ambiguity, never evidence of death.
            Err(_) => return Ok(None),
        };
        if age < LEGACY_LOCK_MIGRATION_MIN_AGE
            || !directory_is_empty(guard_path)?
            || !can_prove_no_other_crabcode_generation_process(owner_pid)
        {
            return Ok(None);
        }
        let after = fs::symlink_metadata(guard_path).with_context(|| {
            format!(
                "cannot re-inspect ownerless acquisition guard {}",
                guard_path.display()
            )
        })?;
        if !same_legacy_directory_snapshot(guard_path, &before, before_identity.as_ref(), &after)
            || !directory_is_empty(guard_path)?
            || !can_prove_no_other_crabcode_generation_process(owner_pid)
        {
            return Ok(None);
        }
        match fs::create_dir(&records_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot initialize version acquisition guard protocol {}",
                        records_dir.display()
                    )
                });
            }
        }
    }

    assert_real_directory(&records_dir, "version acquisition guard protocol directory")?;
    let mut entries = fs::read_dir(guard_path).with_context(|| {
        format!(
            "cannot enumerate version acquisition guard root {}",
            guard_path.display()
        )
    })?;
    let Some(entry) = entries.next() else {
        bail!("version acquisition guard root lost its protocol directory");
    };
    let entry = entry?;
    if entry.file_name() != ACQUIRE_GUARD_RECORDS_DIR || entries.next().is_some() {
        bail!(
            "version acquisition guard root is compromised: {}",
            guard_path.display()
        );
    }
    Ok(Some(records_dir))
}

fn assert_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {label} {}", path.display()))?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        bail!("{label} is not a real directory: {}", path.display());
    }
    Ok(())
}

fn real_directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
                bail!(
                    "version acquisition guard protocol entry is not a real directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "cannot inspect version acquisition guard protocol {}",
                path.display()
            )
        }),
    }
}

fn valid_uuid_v4(value: &str) -> bool {
    let Ok(uuid) = Uuid::parse_str(value) else {
        return false;
    };
    uuid.get_version_num() == 4 && uuid.to_string() == value
}

fn guard_record_name(record: &AcquireGuardRecord) -> String {
    match record.phase {
        AcquireGuardPhase::Choosing => format!("choosing.{}.json", record.owner_token),
        AcquireGuardPhase::Owner => format!(
            "owner.{}.{}.json",
            record.ticket.as_deref().unwrap_or("invalid"),
            record.owner_token
        ),
    }
}

fn parse_guard_record_name(name: &str) -> Option<(AcquireGuardPhase, Option<u64>, &str)> {
    if let Some(token) = name
        .strip_prefix("choosing.")
        .and_then(|value| value.strip_suffix(".json"))
        .filter(|token| valid_uuid_v4(token))
    {
        return Some((AcquireGuardPhase::Choosing, None, token));
    }
    let body = name.strip_prefix("owner.")?.strip_suffix(".json")?;
    let (ticket, token) = body.split_once('.')?;
    if ticket.is_empty()
        || ticket.starts_with('0')
        || !ticket.bytes().all(|byte| byte.is_ascii_digit())
        || !valid_uuid_v4(token)
    {
        return None;
    }
    Some((AcquireGuardPhase::Owner, ticket.parse().ok(), token))
}

fn read_guard_record(path: &Path, logical_name: &str) -> Result<ParsedAcquireGuardRecord> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "cannot inspect native acquisition guard record {}",
            path.display()
        )
    })?;
    if !metadata.is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_ACQUIRE_GUARD_RECORD_BYTES
    {
        bail!(
            "native acquisition guard record is not a bounded real file: {}",
            path.display()
        );
    }
    let raw = fs::read(path).with_context(|| {
        format!(
            "cannot read native acquisition guard record {}",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&raw).with_context(|| {
        format!(
            "cannot parse native acquisition guard record {}",
            path.display()
        )
    })?;
    let object = value
        .as_object()
        .context("native acquisition guard record is not an object")?;
    let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    if keys
        != [
            "ownerToken",
            "phase",
            "pid",
            "processStartIdentity",
            "protocol",
            "ticket",
        ]
    {
        bail!("native acquisition guard record has an invalid schema");
    }
    let record: AcquireGuardRecord =
        serde_json::from_value(value).context("cannot decode native acquisition guard record")?;
    if record.protocol != ACQUIRE_GUARD_PROTOCOL
        || record.pid <= 1
        || record.process_start_identity.is_empty()
        || record.process_start_identity.len() > 512
        || !valid_uuid_v4(&record.owner_token)
    {
        bail!("native acquisition guard record fields are invalid");
    }
    let Some((name_phase, name_ticket, name_token)) = parse_guard_record_name(logical_name) else {
        bail!("native acquisition guard record filename is invalid");
    };
    let ticket = match record.phase {
        AcquireGuardPhase::Choosing if record.ticket.is_none() => None,
        AcquireGuardPhase::Owner => {
            let ticket = record
                .ticket
                .as_deref()
                .context("native acquisition guard owner has no ticket")?
                .parse::<u64>()
                .context("native acquisition guard owner ticket is invalid")?;
            if ticket == 0 {
                bail!("native acquisition guard owner ticket is zero");
            }
            Some(ticket)
        }
        AcquireGuardPhase::Choosing => {
            bail!("native acquisition guard chooser unexpectedly has a ticket");
        }
    };
    if name_phase != record.phase || name_ticket != ticket || name_token != record.owner_token {
        bail!("native acquisition guard filename does not match its owner record");
    }
    Ok(ParsedAcquireGuardRecord {
        path: path.to_path_buf(),
        raw,
        record,
        ticket,
    })
}

fn write_guard_record_exclusive(path: &Path, record: &AcquireGuardRecord) -> Result<()> {
    let mut bytes =
        serde_json::to_vec(record).context("cannot serialize acquisition guard record")?;
    bytes.push(b'\n');
    // Publish only a fully written inode. Before hard-link publication a crash
    // leaves an explicitly non-owning staging entry; after publication the
    // canonical record is complete. hard_link is O_EXCL on every supported
    // filesystem and never replaces an existing canonical token.
    let records_dir = path
        .parent()
        .context("acquisition guard record has no parent directory")?;
    let staging = records_dir.join(format!("staging.{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = options.mode(0o600);
    }
    let mut file = options.open(&staging).with_context(|| {
        format!(
            "cannot create acquisition guard staging record exclusively: {}",
            staging.display()
        )
    })?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staging);
        return Err(error).with_context(|| {
            format!(
                "cannot persist native acquisition guard staging record {}",
                staging.display()
            )
        });
    }
    drop(file);
    if let Err(error) = fs::hard_link(&staging, path) {
        let _ = fs::remove_file(&staging);
        return Err(error).with_context(|| {
            format!(
                "cannot publish native acquisition guard record exclusively: {}",
                path.display()
            )
        });
    }
    // Failure to remove the non-owning name cannot invalidate the complete
    // canonical hard link. A later scanner safely finishes it.
    let _ = fs::remove_file(&staging);
    Ok(())
}

fn guard_record_is_active(record: &AcquireGuardRecord) -> bool {
    guard_record_is_active_with_observation(record, observe_process(record.pid))
}

fn guard_record_is_active_with_observation(
    record: &AcquireGuardRecord,
    observation: ProcessObservation,
) -> bool {
    match observation {
        ProcessObservation::Dead => false,
        // Permission denied and other observation ambiguity are not proof of
        // death. Sleep and wall-clock changes never enter this decision.
        ProcessObservation::AliveUnknown => true,
        ProcessObservation::AliveWithIdentity(observed) => {
            observed == record.process_start_identity
        }
    }
}

fn retire_observed_guard_record(
    observed: &ParsedAcquireGuardRecord,
    retiring_token: &str,
) -> Result<bool> {
    let current = match fs::read(&observed.path) {
        Ok(current) => current,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot re-read acquisition guard owner {}",
                    observed.path.display()
                )
            });
        }
    };
    if current != observed.raw {
        bail!(
            "native acquisition guard owner changed before retirement: {}",
            observed.path.display()
        );
    }
    let tombstone = suffixed_path(&observed.path, &format!(".retired-by-{retiring_token}"));
    match fs::rename(&observed.path, &tombstone) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot retire native acquisition guard owner {}",
                    observed.path.display()
                )
            });
        }
    }
    let moved = match fs::read(&tombstone) {
        Ok(moved) => moved,
        // A concurrent scanner may finish a fully validated tombstone after
        // our atomic rename. Canonical ownership is already gone.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot verify retired native acquisition guard owner {}",
                    tombstone.display()
                )
            });
        }
    };
    if moved != observed.raw {
        // Never hide or unlink a different record. The unexpected tombstone is
        // deliberately left behind so later scans fail closed.
        bail!(
            "native acquisition guard retirement moved a different owner: {}",
            tombstone.display()
        );
    }
    match fs::remove_file(&tombstone) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot remove retired native acquisition guard owner {}",
                    tombstone.display()
                )
            });
        }
    }
    Ok(true)
}

fn retire_guard_record_if_exact(path: &Path, expected: &AcquireGuardRecord, retiring_token: &str) {
    let logical_name = guard_record_name(expected);
    let Ok(observed) = read_guard_record(path, &logical_name) else {
        return;
    };
    if observed.record == *expected {
        let _ = retire_observed_guard_record(&observed, retiring_token);
    }
}

fn scan_active_guard_records(
    records_dir: &Path,
    retiring_token: &str,
) -> Result<Vec<ParsedAcquireGuardRecord>> {
    let mut active = Vec::new();
    for entry in fs::read_dir(records_dir).with_context(|| {
        format!(
            "cannot enumerate native acquisition guard records {}",
            records_dir.display()
        )
    })? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            bail!(
                "native acquisition guard contains a non-file entry: {}",
                entry.path().display()
            );
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("native acquisition guard filename is not Unicode"))?;
        if let Some(staging_owner) = name
            .strip_prefix("staging.")
            .and_then(|value| value.strip_suffix(".tmp"))
        {
            if !valid_uuid_v4(staging_owner) {
                bail!("native acquisition guard staging owner is invalid");
            }
            // Staging is explicitly non-owning. Removing a live writer's path
            // can only fail that unpublished attempt; it cannot admit two
            // owners. Its already-open inode remains private on POSIX.
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "cannot retire unpublished acquisition guard staging record {}",
                            entry.path().display()
                        )
                    });
                }
            }
            continue;
        }
        let retired_suffix = ".retired-by-";
        let (logical_name, retired) = if let Some(index) = name.rfind(&retired_suffix) {
            let retirement_owner = &name[index + retired_suffix.len()..];
            if !valid_uuid_v4(retirement_owner) {
                bail!("native acquisition guard tombstone owner is invalid");
            }
            (&name[..index], true)
        } else {
            (name.as_str(), false)
        };
        if parse_guard_record_name(logical_name).is_none() {
            bail!("native acquisition guard contains an unknown entry: {name}");
        }
        let parsed = match read_guard_record(&entry.path(), logical_name) {
            Ok(parsed) => parsed,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if retired {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "cannot finish native acquisition guard tombstone {}",
                            entry.path().display()
                        )
                    });
                }
            }
            continue;
        }
        if !guard_record_is_active(&parsed.record) {
            retire_observed_guard_record(&parsed, retiring_token)?;
            continue;
        }
        active.push(parsed);
    }
    Ok(active)
}

fn owner_tuple_precedes(
    candidate: &ParsedAcquireGuardRecord,
    ticket: u64,
    owner_token: &str,
) -> bool {
    candidate.ticket.is_some_and(|candidate_ticket| {
        candidate_ticket < ticket
            || (candidate_ticket == ticket && candidate.record.owner_token.as_str() < owner_token)
    })
}

fn prepare_locks_directory(locks_dir: &Path) -> Result<PathBuf> {
    if !locks_dir.is_absolute() {
        bail!("native generation locks directory must be absolute");
    }
    fs::create_dir_all(locks_dir).with_context(|| {
        format!(
            "cannot create native locks directory {}",
            locks_dir.display()
        )
    })?;
    let metadata = fs::symlink_metadata(locks_dir).with_context(|| {
        format!(
            "cannot inspect native locks directory {}",
            locks_dir.display()
        )
    })?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        bail!("native locks path must be a real directory");
    }
    Ok(locks_dir.to_path_buf())
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn directory_is_empty(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("cannot enumerate legacy PID lock {}", path.display()))?;
    Ok(entries.next().is_none())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectorySnapshotIdentity {
    volume: u64,
    file: u64,
}

fn same_legacy_directory_snapshot(
    path: &Path,
    before: &fs::Metadata,
    before_identity: Option<&DirectorySnapshotIdentity>,
    after: &fs::Metadata,
) -> bool {
    if !before.is_dir()
        || !after.is_dir()
        || metadata_is_reparse_point(before)
        || metadata_is_reparse_point(after)
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    before_identity.is_some()
        && before_identity.copied() == directory_snapshot_identity(path, after)
}

#[cfg(unix)]
fn directory_snapshot_identity(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Option<DirectorySnapshotIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(DirectorySnapshotIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn directory_snapshot_identity(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Option<DirectorySnapshotIdentity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: `wide` is NUL-terminated and lives through CreateFileW. The
    // returned handle is closed exactly once on every successful-open path.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let inspected = unsafe { GetFileInformationByHandle(handle, &raw mut info) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if inspected == 0 {
        return None;
    }
    Some(DirectorySnapshotIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
fn directory_snapshot_identity(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Option<DirectorySnapshotIdentity> {
    None
}

fn looks_like_crabcode_generation_process(description: &str) -> bool {
    // `ps` rows are `comm args`; only those first two tokens can identify the
    // executable. Scanning every argument misclassifies a Codex host's
    // `--working-dir .../CrabCode` as a running CrabCode generation.
    let has_crabcode_executable = description.split_whitespace().take(2).any(|value| {
        let value = value.trim_matches(['\'', '"']);
        Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("crabcode") || name.eq_ignore_ascii_case("crabcode.exe")
            })
    });
    if has_crabcode_executable {
        return true;
    }

    let normalized = description.replace('\\', "/").to_ascii_lowercase();
    let mut remainder = normalized.as_str();
    while let Some(index) = remainder.find("/versions/") {
        let after_prefix = &remainder[index + "/versions/".len()..];
        let end = after_prefix
            .find(|character: char| {
                character == '/'
                    || character.is_whitespace()
                    || character == '"'
                    || character == '\''
            })
            .unwrap_or(after_prefix.len());
        if super::is_release_version_segment(&after_prefix[..end]) {
            return true;
        }
        remainder = &after_prefix[end.max(1)..];
        if remainder.is_empty() {
            break;
        }
    }
    false
}

#[cfg(unix)]
fn can_prove_no_other_crabcode_generation_process(owner_pid: u32) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,comm=,args="])
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC0")
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let mut saw_process = false;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        saw_process = true;
        let line = line.trim_start();
        let Some(pid_end) = line.find(char::is_whitespace) else {
            return false;
        };
        let Ok(pid) = line[..pid_end].parse::<u32>() else {
            return false;
        };
        if pid <= 1 || pid == owner_pid {
            continue;
        }
        let description = line[pid_end..].trim_start();
        if description.is_empty() || looks_like_crabcode_generation_process(description) {
            return false;
        }
    }
    saw_process
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn can_prove_no_other_crabcode_generation_process(owner_pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: the snapshot is closed on every path after successful creation;
    // PROCESSENTRY32W has the documented size and remains live for iteration.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut advanced = unsafe { Process32FirstW(snapshot, &raw mut entry) };
    if advanced == 0 {
        unsafe {
            let _ = CloseHandle(snapshot);
        }
        return false;
    }
    loop {
        let pid = entry.th32ProcessID;
        if pid > 1 && pid != owner_pid {
            let end = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let executable_name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if executable_name.eq_ignore_ascii_case("crabcode.exe")
                || executable_name.eq_ignore_ascii_case("crabcode")
            {
                unsafe {
                    let _ = CloseHandle(snapshot);
                }
                return false;
            }
            if executable_name.eq_ignore_ascii_case("bun.exe")
                || executable_name.eq_ignore_ascii_case("bun")
            {
                let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if process.is_null() {
                    unsafe {
                        let _ = CloseHandle(snapshot);
                    }
                    return false;
                }
                let mut path = vec![0u16; 32_768];
                let mut path_len = path.len() as u32;
                let queried = unsafe {
                    QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut path_len)
                };
                unsafe {
                    let _ = CloseHandle(process);
                }
                if queried == 0 {
                    unsafe {
                        let _ = CloseHandle(snapshot);
                    }
                    return false;
                }
                let Ok(path_len) = usize::try_from(path_len) else {
                    unsafe {
                        let _ = CloseHandle(snapshot);
                    }
                    return false;
                };
                if looks_like_crabcode_generation_process(&String::from_utf16_lossy(
                    &path[..path_len],
                )) {
                    unsafe {
                        let _ = CloseHandle(snapshot);
                    }
                    return false;
                }
            }
        }
        advanced = unsafe { Process32NextW(snapshot, &raw mut entry) };
        if advanced == 0 {
            break;
        }
    }
    let enumeration_error = unsafe { GetLastError() };
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    enumeration_error == ERROR_NO_MORE_FILES
}

#[cfg(not(any(unix, windows)))]
fn can_prove_no_other_crabcode_generation_process(_owner_pid: u32) -> bool {
    false
}

fn retire_legacy_lock_directory(path: &Path, owner_pid: u32) -> Result<()> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect legacy PID lock {}", path.display()));
        }
    };
    let before_identity = directory_snapshot_identity(path, &before);
    if before.is_file() && !metadata_is_reparse_point(&before) {
        return Ok(());
    }
    if !before.is_dir() || metadata_is_reparse_point(&before) {
        bail!("PID lock entry is neither a real file nor a real legacy directory");
    }
    let age = SystemTime::now()
        .duration_since(
            before
                .modified()
                .context("legacy PID lock has no modification time")?,
        )
        .context("legacy PID lock modification time is in the future")?;
    if age < LEGACY_LOCK_MIGRATION_MIN_AGE
        || !directory_is_empty(path)?
        || !can_prove_no_other_crabcode_generation_process(owner_pid)
    {
        bail!("legacy PID lock owner cannot be proved inactive");
    }

    let after = fs::symlink_metadata(path)
        .with_context(|| format!("cannot re-inspect legacy PID lock {}", path.display()))?;
    if !same_legacy_directory_snapshot(path, &before, before_identity.as_ref(), &after)
        || !directory_is_empty(path)?
        || !can_prove_no_other_crabcode_generation_process(owner_pid)
    {
        bail!("legacy PID lock changed or lost quiescence during migration");
    }
    fs::remove_dir(path)
        .with_context(|| format!("cannot retire legacy PID lock {}", path.display()))
}

fn read_regular_record_if_present(path: &Path) -> Result<Option<PidLockRecord>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata_is_reparse_point(&metadata)
                || metadata.len() > MAX_LOCK_RECORD_BYTES
            {
                bail!(
                    "PID lock record is not a bounded real file: {}",
                    path.display()
                );
            }
            if metadata.len() == 0 {
                return Ok(None);
            }
            match read_regular_record(path) {
                Ok(record) => Ok(Some(record)),
                // A crashed writer can leave a partial regular record. The
                // shared acquisition guard proves no live writer is mutating it;
                // callers treat this as stale and retire it under that guard.
                Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => Ok(None),
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect PID lock record {}", path.display()))
        }
    }
}

fn regular_file_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
                bail!(
                    "PID lock entry is not a real regular file: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect PID lock entry {}", path.display()))
        }
    }
}

fn read_regular_record(path: &Path) -> Result<PidLockRecord> {
    let bytes = fs::read(path)
        .with_context(|| format!("cannot read PID lock record {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse PID lock record {}", path.display()))
}

fn write_record_exclusive(path: &Path, record: &PidLockRecord) -> Result<()> {
    let mut bytes =
        serde_json::to_vec_pretty(record).context("cannot serialize PID lock record")?;
    bytes.push(b'\n');
    let mut options = OpenOptions::new();
    let _ = options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "cannot create PID lock record exclusively: {}",
            path.display()
        )
    })?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error)
            .with_context(|| format!("cannot persist PID lock record {}", path.display()));
    }
    Ok(())
}

fn remove_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect stale PID lock {}", path.display()))?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        bail!("refusing to remove non-regular PID lock {}", path.display());
    }
    fs::remove_file(path)
        .with_context(|| format!("cannot remove stale PID lock {}", path.display()))
}

fn remove_if_owned(path: &Path, expected: &PidLockRecord) {
    let Ok(current) = read_regular_record(path) else {
        return;
    };
    if current.pid == expected.pid
        && current.owner_token == expected.owner_token
        && current.process_start_identity == expected.process_start_identity
    {
        let _ = fs::remove_file(path);
    }
}

fn unix_time_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("native lease timestamp exceeds u64")
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn protocol_path_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("native generation lease path is not valid Unicode")?;
    #[cfg(windows)]
    {
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{unc}"));
        }
        if let Some(ordinary) = value.strip_prefix(r"\\?\") {
            return Ok(ordinary.to_owned());
        }
    }
    Ok(value.to_owned())
}

#[derive(Debug)]
enum ProcessObservation {
    Dead,
    AliveUnknown,
    AliveWithIdentity(String),
}

fn record_is_active(record: &PidLockRecord) -> bool {
    if record.pid <= 1 {
        return false;
    }
    match observe_process(record.pid) {
        ProcessObservation::Dead => false,
        ProcessObservation::AliveUnknown => true,
        ProcessObservation::AliveWithIdentity(observed) => record
            .process_start_identity
            .as_deref()
            .is_none_or(|expected| expected == observed),
    }
}

#[cfg(target_os = "linux")]
fn observe_process(pid: u32) -> ProcessObservation {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let stat_path = proc_dir.join("stat");
    match fs::read_to_string(&stat_path) {
        Ok(stat_line) => {
            let Some(command_end) = stat_line.rfind(')') else {
                return ProcessObservation::AliveUnknown;
            };
            let Some(start_ticks) = stat_line[command_end + 1..]
                .split_whitespace()
                .nth(19)
                .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            else {
                return ProcessObservation::AliveUnknown;
            };
            ProcessObservation::AliveWithIdentity(format!("linux-proc-startticks:{start_ticks}"))
        }
        Err(_) => probe_unix_process_existence(pid),
    }
}

#[cfg(target_os = "macos")]
fn observe_process(pid: u32) -> ProcessObservation {
    let identity = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC0")
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .output();
    if let Ok(output) = identity {
        if output.status.success() {
            let normalized = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(epoch_seconds) = parse_posix_process_start(&normalized) {
                return ProcessObservation::AliveWithIdentity(format!(
                    "darwin-process-start:{epoch_seconds}"
                ));
            }
        }
    }
    let probe = Command::new("/bin/ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TZ", "UTC0")
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .output();
    match probe {
        Ok(output)
            if output.status.success()
                && !String::from_utf8_lossy(&output.stdout).trim().is_empty() =>
        {
            ProcessObservation::AliveUnknown
        }
        _ => probe_unix_process_existence(pid),
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn probe_unix_process_existence(pid: u32) -> ProcessObservation {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return ProcessObservation::AliveUnknown;
    };
    // SAFETY: signal 0 performs only an existence/permission probe and sends no
    // signal. ESRCH alone proves absence; EPERM and every unfamiliar error are
    // observation ambiguity and therefore fail closed as live.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return ProcessObservation::AliveUnknown;
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        ProcessObservation::Dead
    } else {
        ProcessObservation::AliveUnknown
    }
}

#[cfg(any(test, target_os = "macos"))]
fn parse_posix_process_start(value: &str) -> Option<i64> {
    let mut fields = value.split_whitespace();
    let weekday = fields.next()?;
    if !matches!(
        weekday,
        "Sun" | "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat"
    ) {
        return None;
    }
    let month: i64 = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day = fields.next()?.parse::<i64>().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock.next()?.parse::<i64>().ok()?;
    if clock.next().is_some() {
        return None;
    }
    let year = fields.next()?.parse::<i64>().ok()?;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if fields.next().is_some()
        || !(1..=max_day).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    days_since_epoch
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn observe_process(pid: u32) -> ProcessObservation {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const DOTNET_FILETIME_OFFSET_TICKS: u64 = 504_911_232_000_000_000;

    // SAFETY: OpenProcess returns an owned handle or null. Every successful
    // path below closes it exactly once after GetProcessTimes.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .map(|value| value as u32);
        return match code {
            Some(ERROR_INVALID_PARAMETER) => ProcessObservation::Dead,
            Some(ERROR_ACCESS_DENIED) | None => ProcessObservation::AliveUnknown,
            Some(_) => ProcessObservation::AliveUnknown,
        };
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let observed = unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    if observed == 0 {
        return ProcessObservation::AliveUnknown;
    }
    let filetime = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    let Some(dotnet_ticks) = filetime.checked_add(DOTNET_FILETIME_OFFSET_TICKS) else {
        return ProcessObservation::AliveUnknown;
    };
    ProcessObservation::AliveWithIdentity(format!("win32-process-start:{dotnet_ticks}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn observe_process(_pid: u32) -> ProcessObservation {
    ProcessObservation::AliveUnknown
}

#[cfg(test)]
fn has_active_generation_lease(locks_dir: &Path, version: &str) -> Result<bool> {
    let prefix = format!("{version}.process.");
    for entry in fs::read_dir(locks_dir)
        .with_context(|| format!("cannot enumerate test locks {}", locks_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix(&prefix)
            .and_then(|suffix| suffix.strip_suffix(".lock"))
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            continue;
        };
        let _ = pid;
        if read_regular_record_if_present(&entry.path())?
            .is_some_and(|record| record_is_active(&record))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    const HELPER_GENERATION_ENV: &str = "CRABCODE_TEST_NATIVE_LEASE_GENERATION";
    const HELPER_LOCKS_ENV: &str = "CRABCODE_TEST_NATIVE_LEASE_LOCKS";
    const HELPER_READY_ENV: &str = "CRABCODE_TEST_NATIVE_LEASE_READY";
    const HELPER_RELEASE_ENV: &str = "CRABCODE_TEST_NATIVE_LEASE_RELEASE";
    const HELPER_GUARD_ENV: &str = "CRABCODE_TEST_NATIVE_ACQUIRE_GUARD";
    const HELPER_GUARD_READY_ENV: &str = "CRABCODE_TEST_NATIVE_ACQUIRE_GUARD_READY";

    struct ChildGuard(Option<Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    #[ignore = "subprocess helper for the native generation lease race test"]
    fn generation_lease_process_helper() {
        let Some(generation) = std::env::var_os(HELPER_GENERATION_ENV).map(PathBuf::from) else {
            return;
        };
        let locks = PathBuf::from(std::env::var_os(HELPER_LOCKS_ENV).expect("helper locks"));
        let ready = PathBuf::from(std::env::var_os(HELPER_READY_ENV).expect("helper ready"));
        let release = PathBuf::from(std::env::var_os(HELPER_RELEASE_ENV).expect("helper release"));
        let _lease = GenerationLease::acquire_fresh(&generation, &locks, || Ok(()))
            .expect("helper acquires native lease before simulated TS entry");
        fs::write(&ready, b"ready\n").expect("signal helper ready");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !release.exists() {
            assert!(Instant::now() < deadline, "helper release timeout");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "subprocess helper for the native acquisition guard crash test"]
    fn acquisition_guard_process_helper() {
        let Some(guard_path) = std::env::var_os(HELPER_GUARD_ENV).map(PathBuf::from) else {
            return;
        };
        let ready = PathBuf::from(
            std::env::var_os(HELPER_GUARD_READY_ENV).expect("acquisition guard helper ready"),
        );
        let owner = OwnerIdentity::current_with_token(None).expect("guard helper owner identity");
        let _guard = AcquisitionGuard::try_acquire(&guard_path, &owner)
            .expect("guard helper acquisition")
            .expect("guard helper owns bakery");
        fs::write(&ready, b"ready\n").expect("signal guard helper ready");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn live_native_owner_blocks_gc_while_ts_entry_is_paused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = temp.path().join("versions").join("1.2.3");
        let locks = temp.path().join("state").join("crabcode").join("locks");
        fs::create_dir_all(&generation).expect("generation");
        fs::create_dir_all(&locks).expect("locks");
        fs::write(generation.join("payload"), b"immutable\n").expect("payload");
        let ready = temp.path().join("helper.ready");
        let release = temp.path().join("helper.release");

        let child = Command::new(std::env::current_exe().expect("test executable"))
            .arg("generation_lease_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(HELPER_GENERATION_ENV, &generation)
            .env(HELPER_LOCKS_ENV, &locks)
            .env(HELPER_READY_ENV, &ready)
            .env(HELPER_RELEASE_ENV, &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn native lease owner process");
        let mut child = ChildGuard(Some(child));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            if let Some(status) = child
                .0
                .as_mut()
                .expect("helper child")
                .try_wait()
                .expect("poll helper")
            {
                panic!("native lease helper exited before TS pause: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "native lease helper ready timeout"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // Real second process owns the native record and is paused immediately
        // before the simulated TS entry. GC enters the same version transaction,
        // sees that live record, and must not delete the generation.
        {
            let owner = OwnerIdentity::current_with_token(None).expect("GC owner identity");
            let generation_string = protocol_path_string(&generation).expect("generation path");
            let _transaction =
                VersionTransaction::acquire(&locks, "1.2.3", &generation_string, &owner)
                    .expect("GC version transaction");
            if !has_active_generation_lease(&locks, "1.2.3").expect("active lease check") {
                fs::remove_dir_all(&generation).expect("GC delete unleased generation");
            }
        }
        assert!(generation.exists(), "GC deleted a pre-TS live generation");

        fs::write(&release, b"release\n").expect("release helper");
        let status = child
            .0
            .take()
            .expect("helper child")
            .wait()
            .expect("wait helper");
        assert!(status.success(), "native lease helper failed: {status}");

        // Once the actual owner exits, the durable record is birth-identity
        // stale, so a later GC may reclaim the generation after proving that.
        {
            let owner = OwnerIdentity::current_with_token(None).expect("GC owner identity");
            let generation_string = protocol_path_string(&generation).expect("generation path");
            let _transaction =
                VersionTransaction::acquire(&locks, "1.2.3", &generation_string, &owner)
                    .expect("second GC version transaction");
            if !has_active_generation_lease(&locks, "1.2.3").expect("inactive lease check") {
                fs::remove_dir_all(&generation).expect("reclaim stopped generation");
            }
        }
        assert!(!generation.exists());
    }

    #[test]
    fn generated_owner_tokens_are_lowercase_rfc4122_v4() {
        let token = Uuid::new_v4().to_string();
        assert_eq!(token.len(), 36);
        assert_eq!(token.as_bytes()[14], b'4');
        assert!(matches!(token.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) || byte == b'-')
        );
    }

    #[test]
    fn live_owner_and_old_drop_cannot_be_stolen_or_unlink_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let guard_path = temp.path().join("1.2.3.lock.acquire-guard");
        let owner_a = OwnerIdentity::current_with_token(None).expect("owner A");
        let guard_a = AcquisitionGuard::try_acquire(&guard_path, &owner_a)
            .expect("acquire A")
            .expect("A owns guard");
        let observed_a = guard_a.owner.clone();
        let owner_contender = OwnerIdentity::current_with_token(None).expect("contender");
        assert!(
            AcquisitionGuard::try_acquire(&guard_path, &owner_contender)
                .expect("live contention")
                .is_none(),
            "a live PID/birth owner must never be stolen"
        );
        drop(guard_a);

        let owner_b = OwnerIdentity::current_with_token(None).expect("owner B");
        let guard_b = AcquisitionGuard::try_acquire(&guard_path, &owner_b)
            .expect("acquire B")
            .expect("B owns guard");
        assert!(guard_b.owner.path.exists());
        assert!(
            !retire_observed_guard_record(&observed_a, &owner_a.token)
                .expect("resume stale A retirement"),
            "A's unique owner path is already gone"
        );
        assert!(
            guard_b.owner.path.exists(),
            "resumed A/Drop removed B's replacement owner"
        );
    }

    #[test]
    fn killed_owner_is_recovered_by_pid_birth_proof_without_mtime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let guard_path = temp.path().join("1.2.3.lock.acquire-guard");
        let ready = temp.path().join("guard.ready");
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .arg("acquisition_guard_process_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(HELPER_GUARD_ENV, &guard_path)
            .env(HELPER_GUARD_READY_ENV, &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn acquisition guard owner");
        let mut child = ChildGuard(Some(child));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            if let Some(status) = child
                .0
                .as_mut()
                .expect("guard helper child")
                .try_wait()
                .expect("poll guard helper")
            {
                panic!("guard helper exited before ready: {status}");
            }
            assert!(Instant::now() < deadline, "guard helper ready timeout");
            thread::sleep(Duration::from_millis(10));
        }

        let contender = OwnerIdentity::current_with_token(None).expect("live contender");
        assert!(
            AcquisitionGuard::try_acquire(&guard_path, &contender)
                .expect("live guard contention")
                .is_none()
        );
        child
            .0
            .as_mut()
            .expect("guard helper")
            .kill()
            .expect("kill guard helper");
        let status = child
            .0
            .take()
            .expect("guard helper")
            .wait()
            .expect("wait killed guard helper");
        assert!(!status.success());

        let recovered_owner = OwnerIdentity::current_with_token(None).expect("recovered owner");
        let recovered = AcquisitionGuard::try_acquire(&guard_path, &recovered_owner)
            .expect("recover killed owner")
            .expect("recovered guard");
        assert!(recovered.owner.path.exists());
    }

    #[test]
    fn permission_unknown_is_live_but_pid_reuse_and_dead_are_stale() {
        let owner = OwnerIdentity::current_with_token(None).expect("test owner");
        let record = AcquireGuardRecord {
            protocol: ACQUIRE_GUARD_PROTOCOL.to_owned(),
            phase: AcquireGuardPhase::Owner,
            pid: owner.pid,
            process_start_identity: owner.process_start_identity.clone(),
            owner_token: Uuid::new_v4().to_string(),
            ticket: Some("1".to_owned()),
        };
        assert!(guard_record_is_active_with_observation(
            &record,
            ProcessObservation::AliveUnknown
        ));
        assert!(guard_record_is_active_with_observation(
            &record,
            ProcessObservation::AliveWithIdentity(owner.process_start_identity)
        ));
        assert!(!guard_record_is_active_with_observation(
            &record,
            ProcessObservation::AliveWithIdentity("reused-pid-birth".to_owned())
        ));
        assert!(!guard_record_is_active_with_observation(
            &record,
            ProcessObservation::Dead
        ));
    }

    #[test]
    fn guard_protocol_entry_compromise_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let guard_path = temp.path().join("1.2.3.lock.acquire-guard");
        let owner = OwnerIdentity::current_with_token(None).expect("owner");
        drop(
            AcquisitionGuard::try_acquire(&guard_path, &owner)
                .expect("initialize guard")
                .expect("initial owner"),
        );
        let records = guard_path.join(ACQUIRE_GUARD_RECORDS_DIR);
        fs::create_dir(records.join("attacker-entry")).expect("compromised directory entry");
        let error = AcquisitionGuard::try_acquire(&guard_path, &owner)
            .expect_err("compromised guard must fail closed");
        assert!(error.to_string().contains("non-file entry"));
    }

    #[test]
    fn partial_unpublished_staging_record_is_crash_recoverable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let guard_path = temp.path().join("1.2.3.lock.acquire-guard");
        let owner = OwnerIdentity::current_with_token(None).expect("owner");
        drop(
            AcquisitionGuard::try_acquire(&guard_path, &owner)
                .expect("initialize guard")
                .expect("initial owner"),
        );
        let staging = guard_path
            .join(ACQUIRE_GUARD_RECORDS_DIR)
            .join("staging.33333333-3333-4333-8333-333333333333.tmp");
        fs::write(&staging, b"{\"partial\":").expect("partial staging record");

        let recovered = AcquisitionGuard::try_acquire(&guard_path, &owner)
            .expect("recover partial staging")
            .expect("recovered guard");
        assert!(!staging.exists());
        assert!(recovered.owner.path.exists());
    }

    #[test]
    fn posix_process_start_is_parsed_as_utc_epoch_seconds() {
        assert_eq!(
            parse_posix_process_start("Thu Jan 1 00:00:00 1970"),
            Some(0)
        );
        assert_eq!(
            parse_posix_process_start("Tue Jul 14 12:34:56 2026"),
            Some(1_784_032_496)
        );
        assert_eq!(
            parse_posix_process_start("Thu Feb 29 00:00:00 2024"),
            Some(1_709_164_800)
        );
        assert_eq!(parse_posix_process_start("Sat Feb 29 00:00:00 2025"), None);
    }

    #[test]
    fn generation_process_descriptions_are_conservative() {
        assert!(looks_like_crabcode_generation_process(
            "/opt/CrabCode/versions/1.0.15/bun dist/index.js"
        ));
        assert!(looks_like_crabcode_generation_process(
            r"C:\Users\test\CrabCode\versions\1.0.15\crabcode.exe"
        ));
        assert!(looks_like_crabcode_generation_process("crabcode serve"));
        assert!(!looks_like_crabcode_generation_process(
            "crabcode-app-server serve"
        ));
    }

    #[test]
    fn fresh_legacy_lock_directory_is_never_retired() {
        let temp = tempfile::tempdir().expect("tempdir");
        let legacy = temp.path().join("1.2.3.lock");
        fs::create_dir(&legacy).expect("legacy lock directory");
        let error = retire_legacy_lock_directory(&legacy, std::process::id())
            .expect_err("fresh ownerless directory must fail closed");
        assert!(error.to_string().contains("cannot be proved inactive"));
        assert!(legacy.is_dir());
    }
}
