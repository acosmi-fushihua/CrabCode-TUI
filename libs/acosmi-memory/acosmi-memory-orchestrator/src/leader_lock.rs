// Per-memory-dir leader election for "who runs `memoryRunners.initMemoryRunners
// + drainOnStartup`" across concurrent TUI runtime contexts. Distinct from
// `lock::.consolidate-lock` (which uses mtime to represent
// `last_consolidated_at` with a 1hr default stale window). Here we keep mtime
// as a short-TTL leader heartbeat (60s default), renewed every 30s by the
// leader; non-leaders watchdog at the same cadence and take over when the
// previous leader's PID is dead OR its lease has expired.
//
// Files:
// * `<memory_dir>/.bootstrap-leader-lock` — versioned JSON claim containing
//   PID + opaque token + monotonic epoch + TTL; mtime = lease heartbeat.
// * `<memory_dir>/.bootstrap-leader-epoch` — private, locked monotonic counter
//   that survives release so a same-PID successor still fences its predecessor.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::lock::{is_process_running, BoxError};

pub const LEADER_LOCK_FILE: &str = ".bootstrap-leader-lock";
pub const LEADER_EPOCH_FILE: &str = ".bootstrap-leader-epoch";
const LEADER_LOCK_SCHEMA_VERSION: u64 = 1;

/// Default lease TTL: 60s. Override via `CRABCODE_MEMORY_LEADER_TTL_MS`. Chosen
/// to give one full renew cycle (30s) of slack before a stuck leader's lease
/// becomes stealable — short enough that takeover happens within ~1 minute
/// of a crash.
pub const DEFAULT_LEADER_TTL_MS: u64 = 60_000;

/// Default renew interval: 30s. Override via `CRABCODE_MEMORY_LEADER_RENEW_MS`.
/// Half the TTL so a single missed renewal still leaves margin.
pub const DEFAULT_LEADER_RENEW_INTERVAL_MS: u64 = 30_000;

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct LeaderClaim {
    pub holder_pid: u32,
    pub leader_epoch: u64,
    /// Bearer capability returned only to the successful claimant. It is
    /// persisted in the private lock record for comparison, but deliberately
    /// omitted from status/query serialization and debug output.
    #[serde(skip_serializing)]
    pub leader_token: String,
    pub ttl_ms: u64,
    pub claimed_at_ms: u64,
    pub lease_expires_at_ms: u64,
}

impl fmt::Debug for LeaderClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaderClaim")
            .field("holder_pid", &self.holder_pid)
            .field("leader_epoch", &self.leader_epoch)
            .field("leader_token", &"<redacted>")
            .field("ttl_ms", &self.ttl_ms)
            .field("claimed_at_ms", &self.claimed_at_ms)
            .field("lease_expires_at_ms", &self.lease_expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind")]
pub enum LeaderStatus {
    Vacant,
    HeldByMe { claim: LeaderClaim },
    HeldByOther { claim: LeaderClaim },
    StaleAvailable { stale_claim: LeaderClaim },
}

pub fn leader_lock_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join(LEADER_LOCK_FILE)
}

#[must_use]
pub fn leader_epoch_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join(LEADER_EPOCH_FILE)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LeaderLockRecord {
    schema_version: u64,
    holder_pid: u32,
    leader_epoch: u64,
    leader_token: String,
    ttl_ms: u64,
}

impl LeaderLockRecord {
    fn new(holder_pid: u32, leader_epoch: u64, ttl_ms: u64) -> Self {
        Self {
            schema_version: LEADER_LOCK_SCHEMA_VERSION,
            holder_pid,
            leader_epoch,
            leader_token: uuid::Uuid::new_v4().to_string(),
            ttl_ms,
        }
    }

    fn is_valid(&self) -> bool {
        self.schema_version == LEADER_LOCK_SCHEMA_VERSION
            && self.holder_pid > 0
            && self.leader_epoch > 0
            && !self.leader_token.is_empty()
            && self.ttl_ms > 0
    }

    fn claim(&self, heartbeat_at_ms: u64) -> LeaderClaim {
        LeaderClaim {
            holder_pid: self.holder_pid,
            leader_epoch: self.leader_epoch,
            leader_token: self.leader_token.clone(),
            ttl_ms: self.ttl_ms,
            claimed_at_ms: heartbeat_at_ms,
            lease_expires_at_ms: heartbeat_at_ms.saturating_add(self.ttl_ms),
        }
    }
}

/// Try to claim the leader lease for `memory_dir`. Returns `Some(claim)` when
/// the caller now holds the lease; `None` when another live process holds an
/// unexpired lease.
///
/// Internally (PR-0): the vacant path is a single atomic O_EXCL
/// `create_new` (`try_create_claim`) — "create iff absent" decides the winner
/// without any read-then-write window. On `AlreadyExists` the holder is
/// inspected: if the lease is neither stale nor dead (R1: an empty/unparseable
/// pid on a fresh file counts as live, not dead) the caller loses honestly.
/// A reclaimable (stale/dead) holder is taken over under
/// `leader_mutation_gate()` (a
/// process-global `tokio::sync::Mutex`): the file is re-verified under the
/// gate (a concurrent winner may have refreshed it → lose), then — W-MEMORY-
/// KB-UPLIFT P0 (2026-07-17) — RENAMED to a tomb (`arbitrate_stale_takeover`:
/// atomic cross-process single-winner arbiter + post-rename identity verify +
/// restore-on-mismatch) and recreated via O_EXCL `create_new` while still
/// holding the gate, so exactly one reclaimer wins. Transient Windows
/// contention (scanner hold / delete-pending, `is_transient_contention`) is
/// "lost this round", never a fatal error.
pub async fn try_claim_leader(
    memory_dir: &Path,
    owner_pid: u32,
    ttl_ms: u64,
) -> Result<Option<LeaderClaim>, BoxError> {
    try_claim_leader_with(memory_dir, owner_pid, ttl_ms, is_process_running).await
}

async fn try_claim_leader_with<F>(
    memory_dir: &Path,
    owner_pid: u32,
    ttl_ms: u64,
    is_running: F,
) -> Result<Option<LeaderClaim>, BoxError>
where
    F: Fn(u32) -> bool,
{
    let path = leader_lock_path(memory_dir);
    tokio::fs::create_dir_all(memory_dir).await?;

    // W-MEMORY-EVOLUTION PR-0 (2026-05-29) — race-free claim CAS.
    //
    // Background: before PR-0 the orchestrator held a global
    // `Arc<Mutex<IpcHandler>>` across every request, which serialized all
    // `memory.leader.claim` IPC calls and accidentally masked two concurrency
    // races here. Removing that global lock (the B2-deadlock fix) exposed them.
    //
    // Two interleavings had to be closed:
    //   (R1) Vacant-slot empty-write window: a winning claimant's `create_new`
    //        succeeds but `write_all(pid)` hasn't landed yet, so the file is
    //        empty. A racing claimant reading it parses pid=None; the old code
    //        treated "no parseable pid" as a DEAD holder and unlinked the
    //        in-progress winner's file → both ended up granted.
    //   (R2) Stale-takeover TOCTOU: a claimant decides "reclaimable" from an
    //        early read, then removes/renames the file — but by the time it
    //        acts, a legit winner may have already replaced the file, so it
    //        deletes the winner's fresh lock → both granted.
    //
    // Fixes:
    //   * (R1) An unparseable/empty pid on a *fresh* (non-stale) file is treated
    //     as a LIVE, in-progress claim — never stolen. Only a fresh file with a
    //     parseable-but-dead pid, or any stale file, is reclaimable.
    //   * (R2) The reclaim critical section (re-verify → remove → recreate) runs
    //     under a process-global async gate. This is sound because EVERY
    //     `memory.leader.claim` for a given memory_dir funnels through the single
    //     orchestrator process (Bun workers issue IPC; they never touch the file
    //     directly), so in-process serialization fully arbitrates takeover. The
    //     gate never spans an LLM await, so it does NOT reintroduce the B2
    //     deadlock the global handler Mutex caused. The common vacant fast path
    //     and the live-holder-lose path stay lock-free.
    //
    // Bounded loop so retries after a vanished file / transient Windows
    // contention still terminate; exhausting it returns `None` (caller retries
    // on the next watchdog tick).
    //
    // W-MEMORY-KB-UPLIFT P0 (2026-07-17) — two hardenings on top of PR-0's CAS:
    //   * Transient Windows contention (ERROR_ACCESS_DENIED / SHARING_VIOLATION
    //     from an external scanner holding the file, or a delete-pending
    //     window) is classified by `is_transient_contention` and treated as
    //     "lost this round" — pause + retry within the bounded loop, `Ok(None)`
    //     on exhaustion — never a fatal `Err`. For a lease lock every such
    //     error is semantically "didn't get it now"; the 30s watchdog retries.
    //   * The stale-takeover destructive step is RENAME-to-tomb (atomic
    //     cross-process single winner) + post-rename identity verify +
    //     restore-on-mismatch (`arbitrate_stale_takeover`), replacing
    //     remove+create. This (a) frees the lock name without a Windows
    //     delete-pending window (the winner's `create_new` no longer collides
    //     with a pending delete), and (b) narrows the cross-process TOCTOU
    //     where a reclaimer could silently delete a LEGIT fresh lock created
    //     between its re-verify and its destructive step.
    cleanup_stale_tombs(memory_dir).await;
    for attempt in 0..CLAIM_MAX_ATTEMPTS {
        match try_create_claim(&path, owner_pid, ttl_ms).await? {
            CreateOutcome::Won(claim) => return Ok(Some(claim)),
            CreateOutcome::Exists => { /* fall through to inspect + maybe reclaim */ }
            CreateOutcome::Contended => {
                contention_pause().await;
                continue;
            }
        }

        let observed = match read_existing_contention_aware(&path).await? {
            ReadOutcome::Found(existing) => existing,
            // vanished between create error and read — retry create
            ReadOutcome::Missing => continue,
            ReadOutcome::Contended => {
                contention_pause().await;
                continue;
            }
        };
        let stale = observed.is_stale(ttl_ms);
        // (R1) A holder is reclaimable as "dead" ONLY if its pid is parseable
        // AND not running. An empty/unparseable pid on a fresh file is a claim
        // mid-write — treat as live, lose honestly.
        let dead = match observed.holder_pid {
            Some(holder) => !is_running(holder),
            None => false,
        };
        if !stale && !dead {
            return Ok(None);
        }

        // (R2) Reclaimable → serialize the takeover critical section in-process.
        let _gate = leader_mutation_gate().lock().await;
        // Re-verify under the gate: a winner (or another reclaimer that already
        // held the gate) may have refreshed the lock since our read above.
        match read_existing_contention_aware(&path).await? {
            ReadOutcome::Missing => { /* vanished under gate → fall through to recreate */ }
            ReadOutcome::Contended => {
                contention_pause().await;
                continue;
            }
            ReadOutcome::Found(r) => {
                let r_stale = r.is_stale(ttl_ms);
                let r_dead = match r.holder_pid {
                    Some(holder) => !is_running(holder),
                    None => false,
                };
                if !r_stale && !r_dead {
                    return Ok(None); // refreshed under the gate → lose honestly
                }
                match arbitrate_stale_takeover(&path, owner_pid, attempt, &r).await? {
                    TakeoverOutcome::NameFreed => {}
                    TakeoverOutcome::LostRace => return Ok(None),
                    TakeoverOutcome::Contended => {
                        contention_pause().await;
                        continue;
                    }
                }
            }
        }
        // Recreate WHILE holding the gate so no other reclaimer can interleave.
        // A lock-free vacant racer could still O_EXCL-win in the tiny window
        // between the tomb rename and our create; if so we observe Exists and
        // lose honestly — still exactly one winner overall.
        match try_create_claim(&path, owner_pid, ttl_ms).await? {
            CreateOutcome::Won(claim) => return Ok(Some(claim)),
            // Exists under the gate → a concurrent lock-free vacant racer beat
            // us to the recreate; lose honestly.
            CreateOutcome::Exists => return Ok(None),
            CreateOutcome::Contended => {
                contention_pause().await;
                continue;
            }
        }
    }
    Ok(None)
}

/// W-MEMORY-KB-UPLIFT P0 (2026-07-17) — claim retry bounds. 6 attempts ×
/// 10ms contention pause rides out an external scanner's transient hold
/// without stretching a single IPC claim call past ~60ms worst case.
const CLAIM_MAX_ATTEMPTS: usize = 6;
const CONTENTION_PAUSE_MS: u64 = 10;

async fn contention_pause() {
    tokio::time::sleep(Duration::from_millis(CONTENTION_PAUSE_MS)).await;
}

/// Windows-only transient-contention classifier: ERROR_ACCESS_DENIED(5) /
/// ERROR_SHARING_VIOLATION(32) — an external scanner (Defender / indexer)
/// briefly holding the lock file, or a delete-pending window. On these, a
/// lease-lock operation is semantically "lost this round", never fatal.
/// Non-Windows keeps the strict behaviour: EACCES there is a real permission
/// problem that must surface as an error.
fn is_transient_contention(e: &io::Error) -> bool {
    if !cfg!(windows) {
        return false;
    }
    e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5) | Some(32))
}

fn box_error_is_transient_contention(e: &BoxError) -> bool {
    e.downcast_ref::<io::Error>()
        .is_some_and(is_transient_contention)
}

enum ReadOutcome {
    Found(Existing),
    Missing,
    Contended,
}

async fn read_existing_contention_aware(path: &Path) -> Result<ReadOutcome, BoxError> {
    match read_existing_at(path).await {
        Ok(Some(existing)) => Ok(ReadOutcome::Found(existing)),
        Ok(None) => Ok(ReadOutcome::Missing),
        Err(e) if box_error_is_transient_contention(&e) => Ok(ReadOutcome::Contended),
        Err(e) => Err(e),
    }
}

enum TakeoverOutcome {
    /// The stale lock was renamed away — the lock name is free for `create_new`.
    NameFreed,
    /// The file at the lock path was NOT the stale claim we decided on (a
    /// legit winner replaced it between re-verify and rename) — restored and
    /// lost honestly.
    LostRace,
    /// Transient Windows contention — caller pauses and retries.
    Contended,
}

/// Destructive step of the stale takeover: atomically rename the lock file to
/// a tomb name, verify the tomb still holds the stale claim we decided on
/// (`observed`), then delete the tomb. Rename is the cross-process
/// single-winner arbiter (the second renamer gets `NotFound`), and it frees
/// the lock name without a delete-pending window. Documented residual: the
/// restore path could itself overwrite a third claimant's brand-new lock —
/// that needs three independent processes violating the
/// one-orchestrator-per-memory_dir invariant within microseconds; accepted.
async fn arbitrate_stale_takeover(
    path: &Path,
    owner_pid: u32,
    attempt: usize,
    observed: &Existing,
) -> Result<TakeoverOutcome, BoxError> {
    let tomb = tomb_path(path, owner_pid, attempt);
    match tokio::fs::rename(path, &tomb).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Lost the rename race to another reclaimer — the name is free
            // either way; the caller's `create_new` decides the winner.
            return Ok(TakeoverOutcome::NameFreed);
        }
        Err(e) if is_transient_contention(&e) => return Ok(TakeoverOutcome::Contended),
        Err(e) => return Err(e.into()),
    }
    // Post-rename identity verify: is the tomb the stale artifact we decided
    // on, or did we grab a legit fresh lock written between re-verify and
    // rename?
    let matches_observed = match read_existing_at(&tomb).await {
        Ok(Some(t)) => t.body == observed.body && t.mtime_ms == observed.mtime_ms,
        // Tomb unreadable/vanished: nothing to restore — treat as freed.
        _ => true,
    };
    if matches_observed {
        let _ = tokio::fs::remove_file(&tomb).await; // best-effort tomb cleanup
        return Ok(TakeoverOutcome::NameFreed);
    }
    // Foreign fresh lock grabbed — put it back and lose honestly.
    if let Err(e) = tokio::fs::rename(&tomb, path).await {
        // Restore failed (e.g. a racer already recreated the path). The
        // displaced claim stays in the tomb; log for forensics and lose.
        log::warn!(
            "[leader_lock] takeover restore failed (foreign lock left at {}): {e}",
            tomb.display()
        );
    }
    Ok(TakeoverOutcome::LostRace)
}

fn tomb_path(path: &Path, owner_pid: u32, attempt: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(LEADER_LOCK_FILE);
    path.with_file_name(format!(
        "{file_name}.tomb-{owner_pid}-{attempt}-{}",
        now_ms()
    ))
}

/// Best-effort cleanup of tombs older than 60s (a takeover winner deletes its
/// tomb immediately; leftovers only exist when a process died between rename
/// and delete). Age-gated so an in-flight arbitration's tomb is never touched.
async fn cleanup_stale_tombs(memory_dir: &Path) {
    const TOMB_MAX_AGE_MS: u64 = 60_000;
    let Ok(mut entries) = tokio::fs::read_dir(memory_dir).await else {
        return;
    };
    let tomb_prefix = format!("{LEADER_LOCK_FILE}.tomb-");
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&tomb_prefix) {
            continue;
        }
        let aged_out = match entry.metadata().await {
            Ok(md) => md
                .modified()
                .ok()
                .and_then(|t| system_time_to_ms(t).ok())
                .is_some_and(|ms| now_ms().saturating_sub(ms) >= TOMB_MAX_AGE_MS),
            Err(_) => false,
        };
        if aged_out {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// Process-global gate serializing every read-check-mutate leader operation.
///
/// The vacant `create_new` fast path remains lock-free because O_EXCL is the
/// arbiter and cannot overwrite an existing owner. Stale takeover, renew, and
/// release hold this gate across observation and mutation so an old owner
/// cannot refresh or unlink a newer in-process claimant.
fn leader_mutation_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GATE
}

enum CreateOutcome {
    Won(LeaderClaim),
    /// O_EXCL create failed with AlreadyExists — caller inspects the holder.
    Exists,
    /// W-MEMORY-KB-UPLIFT P0: transient Windows contention (scanner hold /
    /// delete-pending) — caller pauses and retries within its bounded loop.
    Contended,
}

/// Attempt the atomic O_EXCL create-and-write of the leader lock file. Success
/// means this caller is the leader. `AlreadyExists` → `Exists` (caller decides
/// whether to reclaim).
async fn try_create_claim(
    path: &Path,
    owner_pid: u32,
    ttl_ms: u64,
) -> Result<CreateOutcome, BoxError> {
    use tokio::io::AsyncWriteExt;
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_EXCL: atomic "create iff absent"
        .open(path)
        .await
    {
        Ok(mut file) => {
            let memory_dir = path
                .parent()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "leader lock has no parent")
                })?
                .to_path_buf();
            let leader_epoch =
                match tokio::task::spawn_blocking(move || allocate_leader_epoch(&memory_dir)).await
                {
                    Ok(Ok(epoch)) => epoch,
                    Ok(Err(error)) => {
                        drop(file);
                        let _ = tokio::fs::remove_file(path).await;
                        return Err(error);
                    }
                    Err(error) => {
                        drop(file);
                        let _ = tokio::fs::remove_file(path).await;
                        return Err(error.into());
                    }
                };
            let record = LeaderLockRecord::new(owner_pid, leader_epoch, ttl_ms);
            let body = serde_json::to_vec(&record)?;
            if let Err(error) = async {
                file.write_all(&body).await?;
                file.flush().await?;
                file.sync_all().await
            }
            .await
            {
                drop(file);
                // This path was created by this function with O_EXCL and no
                // valid claim was ever returned. Best-effort cleanup leaves a
                // fresh unparsable file fail-closed if unlink itself fails.
                let _ = tokio::fs::remove_file(path).await;
                return Err(error.into());
            }
            drop(file);
            if let Err(error) = secure_private_file(path) {
                let _ = tokio::fs::remove_file(path).await;
                return Err(error);
            }
            let claimed_at_ms = match mtime_ms_of(path).await {
                Ok(value) => value,
                Err(error) => {
                    let _ = tokio::fs::remove_file(path).await;
                    return Err(error);
                }
            };
            Ok(CreateOutcome::Won(record.claim(claimed_at_ms)))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(CreateOutcome::Exists),
        Err(e) if is_transient_contention(&e) => Ok(CreateOutcome::Contended),
        Err(e) => Err(e.into()),
    }
}

fn allocate_leader_epoch(memory_dir: &Path) -> Result<u64, BoxError> {
    let path = leader_epoch_path(memory_dir);
    let existed = path.exists();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Preserve the current epoch until the exclusive lock is held; the
        // value is truncated explicitly only after computing its successor.
        .truncate(false)
        .open(&path)?;
    secure_private_file(&path)?;
    file.lock_exclusive()?;

    let result = (|| -> Result<u64, BoxError> {
        file.seek(SeekFrom::Start(0))?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        let current = if raw.trim().is_empty() {
            0
        } else {
            raw.trim().parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid leader epoch file {}: {error}", path.display()),
                )
            })?
        };
        let next = current
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "leader epoch exhausted"))?;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(format!("{next}\n").as_bytes())?;
        file.sync_all()?;
        if !existed {
            sync_parent_dir(memory_dir)?;
        }
        Ok(next)
    })();

    let unlock_result = file.unlock();
    match (result, unlock_result) {
        (Ok(epoch), Ok(())) => Ok(epoch),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
    }
}

fn secure_private_file(path: &Path) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn sync_parent_dir(path: &Path) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()?;
    }
    Ok(())
}

/// Renew the lease only when PID, opaque token, and epoch all match the live,
/// unexpired record. Returns `None` for a legacy record or any stale fence.
///
/// The shared mutation gate spans PID revalidation and overwrite, preventing
/// a stale owner from overwriting a takeover performed by this orchestrator.
pub async fn renew_leader_lease(
    memory_dir: &Path,
    owner_pid: u32,
    leader_token: &str,
    leader_epoch: u64,
    ttl_ms: u64,
) -> Result<Option<LeaderClaim>, BoxError> {
    let path = leader_lock_path(memory_dir);
    // W-MEMORY-KB-UPLIFT P0: transient Windows contention on the read/write is
    // retried briefly; sustained contention returns `Ok(false)` ("act as
    // lost") rather than an error — the caller steps down, releases, and the
    // watchdog re-elects within one tick. Conservative direction: never keeps
    // a lease it cannot prove, never dual-leaders.
    for _ in 0..3 {
        let result = {
            let _gate = leader_mutation_gate().lock().await;
            renew_leader_lease_once(&path, owner_pid, leader_token, leader_epoch, ttl_ms).await
        };
        match result {
            Ok(renewed) => return Ok(renewed),
            Err(e) if box_error_is_transient_contention(&e) => contention_pause().await,
            Err(e) => return Err(e),
        }
    }
    log::warn!(
        "[leader_lock] renew hit sustained transient contention; treating lease as lost (re-election on next tick)"
    );
    Ok(None)
}

async fn renew_leader_lease_once(
    path: &Path,
    owner_pid: u32,
    leader_token: &str,
    leader_epoch: u64,
    ttl_ms: u64,
) -> Result<Option<LeaderClaim>, BoxError> {
    let observed = match read_existing_at(path).await? {
        Some(existing) => existing,
        None => return Ok(None),
    };
    let Some(mut record) = observed.record else {
        return Ok(None);
    };
    if record.holder_pid != owner_pid
        || record.leader_token != leader_token
        || record.leader_epoch != leader_epoch
        || is_lease_stale(observed.mtime_ms, record.ttl_ms)
    {
        return Ok(None);
    }
    record.ttl_ms = ttl_ms;
    tokio::fs::write(path, serde_json::to_vec(&record)?).await?;
    secure_private_file(path)?;
    let claimed_at_ms = mtime_ms_of(path).await?;
    Ok(Some(record.claim(claimed_at_ms)))
}

/// Release the lease iff PID, opaque token, and epoch match the current live
/// record. Returns `false` for missing, expired, legacy, or stale generations.
///
/// Uses unlink (not the seconds-precision rollback that `lock.rs::rollback`
/// does for `last_consolidated_at`) because there is no `last_held_at`
/// semantic to preserve here — the next claimer just creates a fresh file.
pub async fn release_leader(
    memory_dir: &Path,
    owner_pid: u32,
    leader_token: &str,
    leader_epoch: u64,
) -> Result<bool, BoxError> {
    let path = leader_lock_path(memory_dir);
    // W-MEMORY-KB-UPLIFT P0: best-effort semantics extended to transient
    // Windows contention — retry briefly, then give up quietly (the lease goes
    // stale within TTL and the next claimant reclaims it).
    for _ in 0..3 {
        let result = {
            let _gate = leader_mutation_gate().lock().await;
            release_leader_once(&path, owner_pid, leader_token, leader_epoch).await
        };
        match result {
            Ok(released) => return Ok(released),
            Err(e) if box_error_is_transient_contention(&e) => contention_pause().await,
            Err(e) => return Err(e),
        }
    }
    log::warn!(
        "[leader_lock] release hit sustained transient contention; leaving lease to expire via TTL"
    );
    Ok(false)
}

async fn release_leader_once(
    path: &Path,
    owner_pid: u32,
    leader_token: &str,
    leader_epoch: u64,
) -> Result<bool, BoxError> {
    let observed = match read_existing_at(path).await? {
        Some(existing) => existing,
        None => return Ok(false),
    };
    let Some(record) = observed.record else {
        return Ok(false);
    };
    if record.holder_pid != owner_pid
        || record.leader_token != leader_token
        || record.leader_epoch != leader_epoch
        || is_lease_stale(observed.mtime_ms, record.ttl_ms)
    {
        // Someone else holds it — they own its lifecycle, not us.
        return Ok(false);
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Validate a live leader bearer fence without mutating the lease.
///
/// Runner delivery endpoints use this as their common authorization gate. A
/// legacy PID-only lock, malformed record, expired heartbeat, dead holder,
/// wrong token, or wrong epoch all fail closed.
pub async fn validate_leader_fence(
    memory_dir: &Path,
    leader_token: &str,
    leader_epoch: u64,
) -> Result<Option<LeaderClaim>, BoxError> {
    if leader_token.is_empty() || leader_epoch == 0 {
        return Ok(None);
    }
    let Some(observed) = read_existing_at(&leader_lock_path(memory_dir)).await? else {
        return Ok(None);
    };
    let Some(record) = observed.record else {
        return Ok(None);
    };
    if record.leader_token != leader_token
        || record.leader_epoch != leader_epoch
        || is_lease_stale(observed.mtime_ms, record.ttl_ms)
        || !is_process_running(record.holder_pid)
    {
        return Ok(None);
    }
    Ok(Some(record.claim(observed.mtime_ms)))
}

/// Read-only query: what is the current leader state from `my_pid`'s point of
/// view? Used by TUI helper text (DreamSpace `LeaderStatusBadge`) so users can
/// see whether the current window's worker is the active dream runner.
pub async fn query_leader_status(
    memory_dir: &Path,
    my_pid: u32,
    ttl_ms: u64,
) -> Result<LeaderStatus, BoxError> {
    query_leader_status_with(memory_dir, my_pid, ttl_ms, is_process_running).await
}

async fn query_leader_status_with<F>(
    memory_dir: &Path,
    my_pid: u32,
    ttl_ms: u64,
    is_running: F,
) -> Result<LeaderStatus, BoxError>
where
    F: Fn(u32) -> bool,
{
    let path = leader_lock_path(memory_dir);
    let observed = match read_existing_at(&path).await? {
        Some(existing) => existing,
        None => return Ok(LeaderStatus::Vacant),
    };
    let holder_pid = match observed.holder_pid {
        Some(pid) => pid,
        // Unparseable file content = treat as vacant (corruption recovery).
        None => return Ok(LeaderStatus::Vacant),
    };
    let effective_ttl_ms = observed
        .record
        .as_ref()
        .map_or(ttl_ms, |record| record.ttl_ms);
    let claim = observed.record.as_ref().map_or_else(
        || LeaderClaim {
            holder_pid,
            leader_epoch: 0,
            leader_token: String::new(),
            ttl_ms: effective_ttl_ms,
            claimed_at_ms: observed.mtime_ms,
            lease_expires_at_ms: observed.mtime_ms.saturating_add(effective_ttl_ms),
        },
        |record| record.claim(observed.mtime_ms),
    );
    let stale = is_lease_stale(observed.mtime_ms, effective_ttl_ms);
    let alive = is_running(holder_pid);
    if stale || !alive {
        return Ok(LeaderStatus::StaleAvailable { stale_claim: claim });
    }
    if holder_pid == my_pid {
        Ok(LeaderStatus::HeldByMe { claim })
    } else {
        Ok(LeaderStatus::HeldByOther { claim })
    }
}

#[derive(Debug)]
struct Existing {
    mtime_ms: u64,
    holder_pid: Option<u32>,
    record: Option<LeaderLockRecord>,
    body: String,
}

impl Existing {
    fn is_stale(&self, legacy_ttl_ms: u64) -> bool {
        let ttl_ms = self
            .record
            .as_ref()
            .map_or(legacy_ttl_ms, |record| record.ttl_ms);
        is_lease_stale(self.mtime_ms, ttl_ms)
    }
}

async fn read_existing_at(path: &Path) -> Result<Option<Existing>, BoxError> {
    let (metadata_result, body_result) =
        tokio::join!(tokio::fs::metadata(path), tokio::fs::read_to_string(path));

    let metadata = match metadata_result {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let body = match body_result {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let record = serde_json::from_str::<LeaderLockRecord>(&body)
        .ok()
        .filter(LeaderLockRecord::is_valid);
    let holder_pid = record
        .as_ref()
        .map(|record| record.holder_pid)
        .or_else(|| parse_pid(&body));
    Ok(Some(Existing {
        mtime_ms: system_time_to_ms(metadata.modified()?)?,
        holder_pid,
        record,
        body,
    }))
}

async fn mtime_ms_of(path: &Path) -> Result<u64, BoxError> {
    let metadata = tokio::fs::metadata(path).await?;
    system_time_to_ms(metadata.modified()?)
}

fn parse_pid(raw: &str) -> Option<u32> {
    raw.trim().parse::<u32>().ok()
}

fn system_time_to_ms(time: SystemTime) -> Result<u64, BoxError> {
    Ok(time.duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn is_lease_stale(mtime_ms: u64, ttl_ms: u64) -> bool {
    now_ms().saturating_sub(mtime_ms) >= ttl_ms
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;

    fn pid(p: u32) -> u32 {
        p
    }

    fn read_lock_record(memory_dir: &Path) -> LeaderLockRecord {
        serde_json::from_str(
            &fs::read_to_string(leader_lock_path(memory_dir)).expect("leader lock body"),
        )
        .expect("versioned leader lock record")
    }

    #[tokio::test]
    async fn claim_renew_release_happy_path() {
        let dir = TempDir::new().unwrap();
        // Initial claim.
        let claim = try_claim_leader_with(dir.path(), pid(70_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("first claim should grant");
        assert_eq!(claim.holder_pid, 70_001);
        assert_eq!(claim.leader_epoch, 1);
        assert!(!claim.leader_token.is_empty());
        assert!(claim.claimed_at_ms > 0);
        assert_eq!(claim.lease_expires_at_ms - claim.claimed_at_ms, 60_000);

        // Same owner renews — should succeed.
        let renewed = renew_leader_lease(
            dir.path(),
            70_001,
            &claim.leader_token,
            claim.leader_epoch,
            60_000,
        )
        .await
        .unwrap()
        .expect("owner can renew its own lease");
        assert_eq!(renewed.holder_pid, 70_001);
        assert_eq!(renewed.leader_epoch, claim.leader_epoch);
        assert_eq!(renewed.leader_token, claim.leader_token);
        assert_eq!(renewed.lease_expires_at_ms - renewed.claimed_at_ms, 60_000);

        // Release unlinks the file.
        assert!(release_leader(
            dir.path(),
            70_001,
            &renewed.leader_token,
            renewed.leader_epoch,
        )
        .await
        .unwrap());
        assert!(!leader_lock_path(dir.path()).exists());

        // After release, a different PID can claim immediately.
        let next = try_claim_leader_with(dir.path(), pid(70_002), 60_000, |_| true)
            .await
            .unwrap()
            .expect("post-release claim should grant");
        assert_eq!(next.holder_pid, 70_002);
        assert!(next.leader_epoch > claim.leader_epoch);
    }

    #[tokio::test]
    async fn token_epoch_fence_rejects_forgery_and_old_same_pid_owner() {
        let dir = TempDir::new().unwrap();
        let owner_pid = std::process::id();
        let first = try_claim_leader_with(dir.path(), owner_pid, 60_000, |_| true)
            .await
            .unwrap()
            .expect("first claim");

        assert!(renew_leader_lease(
            dir.path(),
            owner_pid,
            "forged-token",
            first.leader_epoch,
            60_000,
        )
        .await
        .unwrap()
        .is_none());
        assert!(renew_leader_lease(
            dir.path(),
            owner_pid,
            &first.leader_token,
            first.leader_epoch + 1,
            60_000,
        )
        .await
        .unwrap()
        .is_none());
        assert!(
            !release_leader(dir.path(), owner_pid, "forged-token", first.leader_epoch,)
                .await
                .unwrap()
        );
        assert!(leader_lock_path(dir.path()).exists());

        assert!(release_leader(
            dir.path(),
            owner_pid,
            &first.leader_token,
            first.leader_epoch,
        )
        .await
        .unwrap());
        let second = try_claim_leader_with(dir.path(), owner_pid, 60_000, |_| true)
            .await
            .unwrap()
            .expect("same PID may obtain a new generation");
        assert!(second.leader_epoch > first.leader_epoch);
        assert_ne!(second.leader_token, first.leader_token);

        assert!(
            renew_leader_lease(
                dir.path(),
                owner_pid,
                &first.leader_token,
                first.leader_epoch,
                60_000,
            )
            .await
            .unwrap()
            .is_none(),
            "an old same-PID owner must be fenced"
        );
        assert!(
            !release_leader(
                dir.path(),
                owner_pid,
                &first.leader_token,
                first.leader_epoch,
            )
            .await
            .unwrap(),
            "an old same-PID owner must not unlink the new generation"
        );
        assert!(
            validate_leader_fence(dir.path(), &second.leader_token, second.leader_epoch)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn legacy_pid_lock_is_observable_but_never_authorizes_fenced_mutation() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), std::process::id().to_string())
            .await
            .unwrap();

        assert!(validate_leader_fence(dir.path(), "legacy-has-no-token", 1)
            .await
            .unwrap()
            .is_none());
        assert!(renew_leader_lease(
            dir.path(),
            std::process::id(),
            "legacy-has-no-token",
            1,
            60_000,
        )
        .await
        .unwrap()
        .is_none());
        assert!(
            !release_leader(dir.path(), std::process::id(), "legacy-has-no-token", 1,)
                .await
                .unwrap()
        );
        let status = query_leader_status_with(dir.path(), std::process::id(), 60_000, |_| true)
            .await
            .unwrap();
        let serialized = serde_json::to_value(status).unwrap();
        assert!(
            serialized.pointer("/claim/leader_token").is_none(),
            "leader query must never disclose a token"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn competing_claim_grants_one_owner() {
        let dir = TempDir::new().unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let first_dir = dir.path().to_path_buf();
        let first_barrier = Arc::clone(&barrier);
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            try_claim_leader_with(&first_dir, pid(71_001), 60_000, |_| true)
                .await
                .unwrap()
        });

        let second_dir = dir.path().to_path_buf();
        let second_barrier = Arc::clone(&barrier);
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            try_claim_leader_with(&second_dir, pid(71_002), 60_000, |_| true)
                .await
                .unwrap()
        });

        let granted = [first.await.unwrap(), second.await.unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();

        assert_eq!(granted, 1, "exactly one of two competing claims must win");
        let lock = read_lock_record(dir.path());
        assert!(
            lock.holder_pid == 71_001 || lock.holder_pid == 71_002,
            "winning PID must be one of the two contenders, got {lock:?}"
        );
    }

    // ── W-MEMORY-EVOLUTION PR-0 (2026-05-29) — race-fix regression locks ──

    /// (R1) A *fresh* lock file with empty/unparseable content represents a
    /// winning claimant whose O_EXCL create succeeded but whose `write_all(pid)`
    /// hasn't landed yet. A racing claimant MUST NOT steal it. Pre-PR-0 the
    /// pid=None → `alive=false` logic treated this as a dead holder and unlinked
    /// it, producing two winners. Deterministically reproduced by seeding an
    /// empty fresh file.
    #[tokio::test]
    async fn claim_does_not_steal_fresh_empty_in_progress_lock() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        // Empty content + fresh mtime (written just now).
        tokio::fs::write(leader_lock_path(dir.path()), "")
            .await
            .unwrap();

        // is_running=true is irrelevant — the pid is unparseable; the fresh
        // mtime alone must protect the in-progress claim.
        let claim = try_claim_leader_with(dir.path(), pid(90_001), 60_000, |_| true)
            .await
            .unwrap();
        assert!(
            claim.is_none(),
            "must NOT steal a fresh in-progress (empty) lock"
        );
        // File must be untouched — not unlinked, not overwritten.
        assert_eq!(
            fs::read_to_string(leader_lock_path(dir.path())).unwrap(),
            ""
        );
    }

    /// Counterpart to the above: an empty/unparseable file that is also STALE
    /// (old mtime) is genuine corruption past its lease and MUST be reclaimable
    /// (no permanent wedge if a claimant crashed mid-write long ago).
    #[tokio::test]
    async fn claim_takes_over_stale_empty_lock() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), "")
            .await
            .unwrap();
        set_file_mtime(
            leader_lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        let claim = try_claim_leader_with(dir.path(), pid(91_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("stale empty lock must be reclaimable (corruption recovery)");
        assert_eq!(claim.holder_pid, 91_001);
        assert_eq!(read_lock_record(dir.path()).holder_pid, 91_001);
    }

    /// (R2) N concurrent claimants all observing the SAME pre-seeded stale/dead
    /// lock must grant exactly one. Pre-PR-0 the unlink-takeover let two
    /// claimants ping-pong-delete each other's fresh write; PR-0's
    /// `leader_mutation_gate()` (process-global `tokio::sync::Mutex`) serializes the
    /// read→remove→`create_new` O_EXCL takeover and re-verifies the file under
    /// the gate, so exactly one wins. Uses the same `owner_pid` for all
    /// contenders to also exercise the dead/stale re-verify path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_takeover_of_stale_lock_grants_exactly_one() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        // Seed a stale lock (old mtime) held by some prior pid.
        tokio::fs::write(leader_lock_path(dir.path()), "99000")
            .await
            .unwrap();
        set_file_mtime(
            leader_lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        // W-MEMORY-KB-UPLIFT P0: mutual exclusion is asserted STRICTLY (never
        // two winners in any round); liveness is asserted over bounded retry
        // rounds — under sustained external contention (an AV scanner holding
        // the file) a whole round can honestly grant zero, and the production
        // watchdog simply retries next tick. 3 rounds is far beyond anything
        // observed.
        let mut total_granted = 0usize;
        for _round in 0..3 {
            let barrier = Arc::new(Barrier::new(4));
            let mut handles = Vec::new();
            for _ in 0..4 {
                let d = dir.path().to_path_buf();
                let b = Arc::clone(&barrier);
                handles.push(tokio::spawn(async move {
                    b.wait().await;
                    // All contenders "alive" (is_running=true) so the only path
                    // to a grant is the stale-lease takeover arbiter, not
                    // dead-pid.
                    try_claim_leader_with(&d, pid(99_001), 60_000, |_| true)
                        .await
                        .unwrap()
                }));
            }
            let mut round_granted = 0usize;
            for h in handles {
                if h.await.unwrap().is_some() {
                    round_granted += 1;
                }
            }
            assert!(
                round_granted <= 1,
                "mutual exclusion violated: {round_granted} winners in one round"
            );
            total_granted += round_granted;
            if total_granted == 1 {
                break;
            }
        }
        assert_eq!(
            total_granted, 1,
            "exactly one stale-takeover claim must win within bounded rounds"
        );
        assert_eq!(read_lock_record(dir.path()).holder_pid, 99_001);
    }

    /// W-MEMORY-KB-UPLIFT P0 — transient-contention classification is
    /// Windows-only: ACCESS_DENIED(5) / SHARING_VIOLATION(32) / any
    /// PermissionDenied count as "lost this round" there; on Unix the same
    /// errors stay fatal (EACCES is a real permission problem).
    #[test]
    fn transient_contention_classifier_is_platform_scoped() {
        let access_denied = io::Error::from_raw_os_error(5);
        let sharing_violation = io::Error::from_raw_os_error(32);
        let perm = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(is_transient_contention(&access_denied), cfg!(windows));
        assert_eq!(is_transient_contention(&sharing_violation), cfg!(windows));
        assert_eq!(is_transient_contention(&perm), cfg!(windows));
        // Never transient anywhere:
        let not_found = io::Error::new(io::ErrorKind::NotFound, "gone");
        assert!(!is_transient_contention(&not_found));
    }

    /// W-MEMORY-KB-UPLIFT P0 — takeover arbiter, identity-match path: the
    /// tomb rename must free the lock name and leave no tomb behind.
    #[tokio::test]
    async fn takeover_arbiter_frees_name_when_identity_matches() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        let path = leader_lock_path(dir.path());
        tokio::fs::write(&path, "99000").await.unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        let observed = match read_existing_at(&path).await.unwrap() {
            Some(existing) => existing,
            None => panic!("seeded lock must be readable"),
        };

        let outcome = arbitrate_stale_takeover(&path, 99_001, 0, &observed)
            .await
            .unwrap();
        assert!(matches!(outcome, TakeoverOutcome::NameFreed));
        assert!(!path.exists(), "stale lock must be renamed away");
        assert!(
            tomb_leftovers(dir.path()).is_empty(),
            "winner must delete its tomb"
        );
    }

    /// W-MEMORY-KB-UPLIFT P0 — takeover arbiter, foreign-lock path: when a
    /// legit winner replaced the file between re-verify and rename, the
    /// arbiter must restore it byte-for-byte and lose honestly (this is the
    /// cross-process TOCTOU the old remove+create takeover could not detect).
    #[tokio::test]
    async fn takeover_arbiter_restores_foreign_fresh_lock_and_loses() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        let path = leader_lock_path(dir.path());
        // The claimant decided on a STALE observation…
        let observed = Existing {
            mtime_ms: 1_700_000_000_000,
            holder_pid: Some(99_000),
            record: None,
            body: "99000".to_owned(),
        };
        // …but by arbitration time a legit winner holds a fresh lock.
        tokio::fs::write(&path, "12345").await.unwrap();

        let outcome = arbitrate_stale_takeover(&path, 99_001, 0, &observed)
            .await
            .unwrap();
        assert!(matches!(outcome, TakeoverOutcome::LostRace));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "12345",
            "foreign fresh lock must be restored intact"
        );
        assert!(
            tomb_leftovers(dir.path()).is_empty(),
            "restore must not leave tombs"
        );
    }

    /// W-MEMORY-KB-UPLIFT P0 — aged tombs (crash between rename and delete)
    /// are swept at claim entry; fresh tombs (an in-flight arbitration) are
    /// never touched.
    #[tokio::test]
    async fn claim_sweeps_aged_tombs_only() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        let aged = dir.path().join(format!("{LEADER_LOCK_FILE}.tomb-1-0-1"));
        let fresh = dir.path().join(format!("{LEADER_LOCK_FILE}.tomb-2-0-2"));
        tokio::fs::write(&aged, "99000").await.unwrap();
        tokio::fs::write(&fresh, "99000").await.unwrap();
        set_file_mtime(&aged, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();

        let claim = try_claim_leader_with(dir.path(), pid(90_101), 60_000, |_| true)
            .await
            .unwrap()
            .expect("vacant claim should grant");
        assert_eq!(claim.holder_pid, 90_101);
        assert!(!aged.exists(), "aged tomb must be swept at claim entry");
        assert!(fresh.exists(), "fresh tomb must be left alone");
    }

    fn tomb_leftovers(dir: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tomb-"))
            })
            .collect()
    }

    #[tokio::test]
    async fn claim_takes_over_dead_pid() {
        let dir = TempDir::new().unwrap();
        // Seed a lock file with a "dead" PID, fresh mtime.
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), "72001")
            .await
            .unwrap();

        // is_running returns false for everyone → takeover proceeds.
        let claim = try_claim_leader_with(dir.path(), pid(72_002), 60_000, |_| false)
            .await
            .unwrap()
            .expect("takeover of dead pid should grant");
        assert_eq!(claim.holder_pid, 72_002);
        assert_eq!(read_lock_record(dir.path()).holder_pid, 72_002);
    }

    #[tokio::test]
    async fn claim_takes_over_stale_lease() {
        let dir = TempDir::new().unwrap();
        // Seed a lock file with an "alive" PID but old mtime (lease expired).
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), "73001")
            .await
            .unwrap();
        // Set mtime to 1970+1700M seconds = ~2023-11 (way past any sane TTL).
        set_file_mtime(
            leader_lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        // is_running returns true (the prior leader is "alive") but the
        // lease itself is stale → takeover proceeds.
        let claim = try_claim_leader_with(dir.path(), pid(73_002), 60_000, |_| true)
            .await
            .unwrap()
            .expect("takeover of stale lease should grant even if pid alive");
        assert_eq!(claim.holder_pid, 73_002);
    }

    #[tokio::test]
    async fn renew_returns_false_after_takeover() {
        let dir = TempDir::new().unwrap();
        // A claims.
        let claim = try_claim_leader_with(dir.path(), pid(74_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("A claims");
        // Simulate B takeover by directly overwriting the file (e.g. B
        // detected A's lease was stale).
        tokio::fs::write(leader_lock_path(dir.path()), "74002")
            .await
            .unwrap();

        // A's renew now returns false — file PID no longer matches.
        let renewed = renew_leader_lease(
            dir.path(),
            74_001,
            &claim.leader_token,
            claim.leader_epoch,
            60_000,
        )
        .await
        .unwrap();
        assert!(
            renewed.is_none(),
            "renew must return false when PID has been overwritten"
        );
        // File content unchanged (renew is fail-safe — does not stomp B).
        assert_eq!(
            fs::read_to_string(leader_lock_path(dir.path())).unwrap(),
            "74002"
        );
    }

    #[tokio::test]
    async fn release_only_unlinks_when_pid_matches() {
        let dir = TempDir::new().unwrap();
        // A holds the lock.
        let claim = try_claim_leader_with(dir.path(), pid(75_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("A claims");

        // B tries to release — should be a no-op (does not delete A's file).
        assert!(
            !release_leader(dir.path(), 75_002, &claim.leader_token, claim.leader_epoch,)
                .await
                .unwrap()
        );
        assert!(
            leader_lock_path(dir.path()).exists(),
            "release with non-matching pid must not unlink"
        );
        assert_eq!(read_lock_record(dir.path()).holder_pid, 75_001);

        // A's release does unlink.
        assert!(
            release_leader(dir.path(), 75_001, &claim.leader_token, claim.leader_epoch,)
                .await
                .unwrap()
        );
        assert!(!leader_lock_path(dir.path()).exists());

        // Idempotent: release on a missing file is OK.
        assert!(
            !release_leader(dir.path(), 75_001, &claim.leader_token, claim.leader_epoch,)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn query_returns_vacant_when_no_file() {
        let dir = TempDir::new().unwrap();
        let status = query_leader_status_with(dir.path(), pid(76_001), 60_000, |_| true)
            .await
            .unwrap();
        assert_eq!(status, LeaderStatus::Vacant);
    }

    #[tokio::test]
    async fn query_returns_held_by_me_when_my_pid_matches() {
        let dir = TempDir::new().unwrap();
        try_claim_leader_with(dir.path(), pid(77_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("claim");
        let status = query_leader_status_with(dir.path(), pid(77_001), 60_000, |_| true)
            .await
            .unwrap();
        match status {
            LeaderStatus::HeldByMe { claim } => assert_eq!(claim.holder_pid, 77_001),
            other => panic!("expected HeldByMe, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_returns_held_by_other_when_pid_differs() {
        let dir = TempDir::new().unwrap();
        try_claim_leader_with(dir.path(), pid(78_001), 60_000, |_| true)
            .await
            .unwrap()
            .expect("claim");
        let status = query_leader_status_with(dir.path(), pid(78_999), 60_000, |_| true)
            .await
            .unwrap();
        match status {
            LeaderStatus::HeldByOther { claim } => assert_eq!(claim.holder_pid, 78_001),
            other => panic!("expected HeldByOther, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_returns_stale_available_when_lease_expired() {
        let dir = TempDir::new().unwrap();
        // Seed an "expired" lock.
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), "79001")
            .await
            .unwrap();
        set_file_mtime(
            leader_lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        // Even though is_running=true, lease is stale → StaleAvailable.
        let status = query_leader_status_with(dir.path(), pid(79_002), 60_000, |_| true)
            .await
            .unwrap();
        match status {
            LeaderStatus::StaleAvailable { stale_claim } => {
                assert_eq!(stale_claim.holder_pid, 79_001);
            }
            other => panic!("expected StaleAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn query_returns_stale_available_when_pid_dead() {
        let dir = TempDir::new().unwrap();
        // Fresh lock file, but is_running=false (PID dead).
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(leader_lock_path(dir.path()), "80001")
            .await
            .unwrap();
        let status = query_leader_status_with(dir.path(), pid(80_002), 60_000, |_| false)
            .await
            .unwrap();
        match status {
            LeaderStatus::StaleAvailable { stale_claim } => {
                assert_eq!(stale_claim.holder_pid, 80_001);
            }
            other => panic!("expected StaleAvailable, got {other:?}"),
        }
    }
}
