use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use filetime::{set_file_mtime, FileTime};
use sysinfo::{Pid, ProcessesToUpdate, System};

pub const LOCK_FILE: &str = ".consolidate-lock";
pub const DEFAULT_HOLDER_STALE_MS: u64 = 60 * 60 * 1000;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockOwner {
    pub holder_pid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockOptions {
    pub holder_stale_ms: u64,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            holder_stale_ms: std::env::var("CRABCODE_MEMORY_LOCK_STALE_MS")
                .ok()
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or(DEFAULT_HOLDER_STALE_MS),
        }
    }
}

pub async fn try_acquire(memory_dir: &Path) -> Result<Option<u64>, BoxError> {
    let owner = LockOwner {
        holder_pid: std::process::id(),
    };
    try_acquire_for(memory_dir, &owner, &LockOptions::default()).await
}

pub async fn try_acquire_for(
    memory_dir: &Path,
    owner: &LockOwner,
    options: &LockOptions,
) -> Result<Option<u64>, BoxError> {
    try_acquire_with_process_checker(memory_dir, owner, options, is_process_running).await
}

/// Mark a consolidation as freshly completed: write the empty-body sentinel
/// (`b""` = "no active holder → available") and let the write syscall stamp a
/// FRESH mtime. The fresh mtime ARMS the ~1h time-gate as "just consolidated"
/// (`last_consolidated_at` reads this mtime), so the next periodic dream is
/// throttled until the interval elapses.
///
/// This is the SOLE success-path release for the `.consolidate-lock`. Both
/// teardown sites (`ResultListener::handle_completed` and
/// `DreamProcessor::process`) call THIS one impl so the success semantics
/// (empty body + fresh mtime) can never drift between them.
pub async fn record_consolidation_complete(memory_dir: &Path) -> Result<(), BoxError> {
    tokio::fs::create_dir_all(memory_dir).await?;
    tokio::fs::write(lock_path(memory_dir), b"").await?;
    Ok(())
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-a (2026-07-16) — like
/// [`record_consolidation_complete`]，但把锁 mtime 盖到 `watermark_ms` 而非
/// 「现在」。用于语料撞 `DREAM_MAX_SESSIONS` 帽、本轮只消费了 touched 会话
/// 前缀的场景：水位线只推进到**已消费最新会话**的 mtime，未消费的积压保持
/// 在水位线之上，下一周期继续排干（grok-build `processed_stems` 语义在
/// mtime 水位线设计下的净室适配）。`watermark_ms == 0` → 等同
/// `record_consolidation_complete`（fresh mtime）。秒级精度（与 `rollback`
/// 一致）：截断至多让水位线低 <1s，最坏重读一个会话，去重管线幂等兜底。
pub async fn record_consolidation_complete_at(
    memory_dir: &Path,
    watermark_ms: u64,
) -> Result<(), BoxError> {
    record_consolidation_complete(memory_dir).await?;
    if watermark_ms > 0 {
        // 秒级**向上**取整（毁灭复核修正）：向下取整会让已消费的最新会话
        // （mtime 带毫秒余数）恒 > 水位线 → 每个周期被重复整理（同素材
        // 白烧 LLM）。向上取整 = 「消费截至此刻」，同一秒内更晚的兄弟
        // 会话最坏被跳过一次（与 rollback 的秒级精度权衡同款，去重管线
        // 幂等兜底）。
        let seconds = watermark_ms.div_ceil(1000) as i64;
        set_file_mtime(lock_path(memory_dir), FileTime::from_unix_time(seconds, 0))?;
    }
    Ok(())
}

/// Read the current lock holder PID (parsed from the file body). `None` when
/// the lock file is missing, empty (rolled-back / released sentinel), or holds
/// an unparseable body. Used by `DreamProcessor::process` to decide whether it
/// actually holds the real lock (so a `forced_skip_lock` virtual-acquire — which
/// writes NO PID — settles to a NO-OP instead of clobbering a foreign holder).
pub async fn current_holder_pid(memory_dir: &Path) -> Result<Option<u32>, BoxError> {
    Ok(read_existing_lock(&lock_path(memory_dir))
        .await?
        .and_then(|existing| existing.holder_pid))
}

pub async fn rollback(memory_dir: &Path, prior_mtime_ms: u64) -> Result<(), BoxError> {
    let path = lock_path(memory_dir);
    if prior_mtime_ms == 0 {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        tokio::fs::write(&path, b"").await?;
        let seconds = (prior_mtime_ms / 1000) as i64;
        set_file_mtime(&path, FileTime::from_unix_time(seconds, 0))?;
        Ok(())
    }
}

pub async fn last_consolidated_at(memory_dir: &Path) -> Result<u64, BoxError> {
    let path = lock_path(memory_dir);
    match tokio::fs::metadata(path).await {
        Ok(metadata) => system_time_to_mtime_ms(metadata.modified()?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

pub fn lock_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join(LOCK_FILE)
}

pub fn is_process_running(pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

/// W-MEMORY-EVOLUTION PR-3 follow-up (2026-05-29) — process-global gate
/// serializing the `.consolidate-lock` takeover critical section. Same
/// rationale as `leader_lock::reclaim_gate`: every consolidate-lock acquire
/// funnels through the single orchestrator process (TS via `memory.lock.
/// acquire` IPC, Rust dream_gate in-process), so in-process serialization
/// fully arbitrates takeover. The gate never spans an LLM await.
fn acquire_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GATE
}

async fn try_acquire_with_process_checker<F>(
    memory_dir: &Path,
    owner: &LockOwner,
    options: &LockOptions,
    is_running: F,
) -> Result<Option<u64>, BoxError>
where
    F: Fn(u32) -> bool,
{
    let path = lock_path(memory_dir);
    tokio::fs::create_dir_all(memory_dir).await?;

    // W-MEMORY-EVOLUTION PR-3 follow-up (2026-05-29) — race-free acquire.
    //
    // The previous `write(path, pid)` + read-back-verify was NOT atomic: two
    // acquirers that ran fully sequentially (A.write→A.read→win, then
    // B.write→B.read→win) BOTH won — a double-consolidation bug that surfaced
    // as the flaky `test_competing_acquire_grants_one_owner` (2 winners). Same
    // race class as the `leader_lock` (PR-0) bugs.
    //
    // Fix: serialize the WHOLE acquire (read → decide → write) under a
    // process-global gate. Sound because every consolidate-lock acquire
    // funnels through the single orchestrator process (TS via `memory.lock.
    // acquire` IPC, Rust dream_gate in-process); the gate never spans an LLM
    // await so it does not reintroduce B2. Unlike `leader_lock`, this lock's
    // **empty file is the rolled-back / released sentinel** (`rollback()`
    // writes `b""`), i.e. "no active holder → available" — NOT a mid-write
    // marker. So we must NOT borrow leader_lock's O_EXCL + "empty=in-progress"
    // shape (it would wrongly refuse to re-acquire a rolled-back lock). Full
    // serialization preserves the original "empty = available" semantics while
    // making read→decide→write atomic, and lets us drop the now-redundant
    // read-back verify (no concurrent writer can interleave under the gate).
    let _gate = acquire_gate().lock().await;

    let observed = read_existing_lock(&path).await?;
    if let Some(existing) = observed.as_ref() {
        // Held iff a parseable, live pid whose lease is not stale. An empty /
        // unparseable holder (rolled-back sentinel) is "available".
        if !existing.is_stale(options.holder_stale_ms)
            && existing.holder_pid.map(&is_running).unwrap_or(false)
        {
            return Ok(None);
        }
    }

    tokio::fs::write(&path, owner.holder_pid.to_string()).await?;
    Ok(Some(
        observed.map(|existing| existing.mtime_ms).unwrap_or(0),
    ))
}

#[derive(Debug)]
struct ExistingLock {
    mtime_ms: u64,
    holder_pid: Option<u32>,
}

impl ExistingLock {
    fn is_stale(&self, holder_stale_ms: u64) -> bool {
        now_ms().saturating_sub(self.mtime_ms) >= holder_stale_ms
    }
}

async fn read_existing_lock(path: &Path) -> Result<Option<ExistingLock>, BoxError> {
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

    Ok(Some(ExistingLock {
        mtime_ms: system_time_to_mtime_ms(metadata.modified()?)?,
        holder_pid: parse_holder_pid(&body),
    }))
}

fn parse_holder_pid(raw: &str) -> Option<u32> {
    raw.trim().parse::<u32>().ok()
}

fn system_time_to_mtime_ms(time: SystemTime) -> Result<u64, BoxError> {
    Ok(time.duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;

    fn owner(pid: u32) -> LockOwner {
        LockOwner { holder_pid: pid }
    }

    fn test_options(stale_ms: u64) -> LockOptions {
        LockOptions {
            holder_stale_ms: stale_ms,
        }
    }

    /// W-MEMORY-SELF-EVOLVE-DGM G3-a — 撞帽语料的水位线只推进到已消费最新
    /// 会话 mtime；0 = 旧语义 fresh mtime。
    #[tokio::test]
    async fn test_record_consolidation_complete_at_stamps_watermark() {
        let dir = TempDir::new().unwrap();
        record_consolidation_complete_at(dir.path(), 1_700_000_000_000)
            .await
            .unwrap();
        assert_eq!(
            last_consolidated_at(dir.path()).await.unwrap(),
            1_700_000_000_000,
            "锁 mtime 盖到给定水位线（秒级精度）"
        );
        // 毫秒余数向上取整：已消费会话（mtime ≤ 水位线）绝不重梦。
        record_consolidation_complete_at(dir.path(), 1_700_000_000_123)
            .await
            .unwrap();
        assert_eq!(
            last_consolidated_at(dir.path()).await.unwrap(),
            1_700_000_001_000,
            "非整秒水位线必须向上取整（floor 会让最新会话每周期重梦）"
        );
        assert_eq!(
            fs::read_to_string(lock_path(dir.path())).unwrap(),
            "",
            "body 仍是空释放哨兵"
        );

        // 0 = fresh mtime（等同 record_consolidation_complete）。生产 settle
        // 时锁体恒为持有者 PID（非空），这里模拟同形态 —— NTFS 对「空文件
        // 写空内容」可能不刷 mtime，纯空→空不是生产会出现的转换。
        fs::write(lock_path(dir.path()), "12345").unwrap();
        set_file_mtime(
            lock_path(dir.path()),
            FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
        record_consolidation_complete_at(dir.path(), 0)
            .await
            .unwrap();
        assert!(
            last_consolidated_at(dir.path()).await.unwrap() > 1_700_000_000_000,
            "0 = fresh mtime（等同 record_consolidation_complete）"
        );
        assert_eq!(fs::read_to_string(lock_path(dir.path())).unwrap(), "");
    }

    #[tokio::test]
    async fn test_acquire_records_owner_pid_and_prior_mtime() {
        let dir = TempDir::new().unwrap();
        let result = try_acquire_with_process_checker(
            dir.path(),
            &owner(42_001),
            &test_options(DEFAULT_HOLDER_STALE_MS),
            |_| false,
        )
        .await
        .unwrap();

        assert_eq!(result, Some(0));
        assert_eq!(fs::read_to_string(lock_path(dir.path())).unwrap(), "42001");
        assert!(last_consolidated_at(dir.path()).await.unwrap() > 0);
    }

    #[tokio::test]
    async fn test_acquire_ms_precision() {
        let dir = TempDir::new().unwrap();
        let mut saw_subsecond = false;

        for i in 0..20 {
            rollback(dir.path(), 0).await.unwrap();
            try_acquire_with_process_checker(
                dir.path(),
                &owner(43_000 + i),
                &test_options(DEFAULT_HOLDER_STALE_MS),
                |_| false,
            )
            .await
            .unwrap();

            let mtime_ms = last_consolidated_at(dir.path()).await.unwrap();
            if mtime_ms % 1000 != 0 {
                saw_subsecond = true;
                break;
            }

            tokio::time::sleep(Duration::from_millis(37)).await;
        }

        assert!(saw_subsecond, "acquire should preserve syscall mtime ms");
    }

    #[tokio::test]
    async fn test_rollback_seconds_precision() {
        let dir = TempDir::new().unwrap();
        try_acquire_with_process_checker(
            dir.path(),
            &owner(44_001),
            &test_options(DEFAULT_HOLDER_STALE_MS),
            |_| false,
        )
        .await
        .unwrap();

        rollback(dir.path(), 1_700_000_123_987).await.unwrap();

        assert_eq!(fs::read_to_string(lock_path(dir.path())).unwrap(), "");
        assert_eq!(last_consolidated_at(dir.path()).await.unwrap() % 1000, 0);
        assert_eq!(
            last_consolidated_at(dir.path()).await.unwrap(),
            1_700_000_123_000
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_competing_acquire_grants_one_owner() {
        let dir = TempDir::new().unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let first_dir = dir.path().to_path_buf();
        let first_barrier = Arc::clone(&barrier);
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            try_acquire_with_process_checker(
                &first_dir,
                &owner(45_001),
                &test_options(DEFAULT_HOLDER_STALE_MS),
                |_| true,
            )
            .await
            .unwrap()
        });

        let second_dir = dir.path().to_path_buf();
        let second_barrier = Arc::clone(&barrier);
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            try_acquire_with_process_checker(
                &second_dir,
                &owner(45_002),
                &test_options(DEFAULT_HOLDER_STALE_MS),
                |_| true,
            )
            .await
            .unwrap()
        });

        let granted = [first.await.unwrap(), second.await.unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();

        assert_eq!(granted, 1);
        let lock_body = fs::read_to_string(lock_path(dir.path())).unwrap();
        assert!(lock_body == "45001" || lock_body == "45002");
    }

    /// W-MEMORY-EVOLUTION PR-3 follow-up — concurrent takeover of a pre-seeded
    /// stale lock must grant exactly one. The previous non-atomic
    /// write+read-verify let multiple reclaimers all win; the reclaim gate +
    /// re-verify closes it. (Same regression lock as leader_lock R2.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_takeover_of_stale_lock_grants_one() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(lock_path(dir.path()), "48000")
            .await
            .unwrap();
        set_file_mtime(
            lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for i in 0..4 {
            let d = dir.path().to_path_buf();
            let b = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                b.wait().await;
                // All contenders "alive" (is_running=true) so the only path to
                // a grant is the stale-lease takeover, not dead-pid.
                //
                // stale_ms = 60_000 (not 10): the seed mtime (2023) is stale
                // relative to 60s, but once a winner overwrites with a FRESH
                // lock it must stay non-stale for the rest of the test so the
                // other serialized acquirers see it held. With stale_ms=10ms,
                // heavy parallel test load could let >10ms elapse between the
                // winner's write and the next acquirer's re-verify, making the
                // fresh lock look stale → a second takeover (flaky). Production
                // stale_ms is 1hr, so a fresh lock is never "instantly stale";
                // 60_000 matches that semantic + the leader_lock test.
                try_acquire_with_process_checker(
                    &d,
                    &owner(48_001 + i),
                    &test_options(60_000),
                    |_| true,
                )
                .await
                .unwrap()
            }));
        }
        let mut granted = 0usize;
        for h in handles {
            if h.await.unwrap().is_some() {
                granted += 1;
            }
        }
        assert_eq!(
            granted, 1,
            "exactly one of four concurrent stale-takeover acquires must win, got {granted}"
        );
    }

    #[tokio::test]
    async fn test_stale_live_pid_recovery() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(lock_path(dir.path()), "46001")
            .await
            .unwrap();
        set_file_mtime(
            lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();

        let prior =
            try_acquire_with_process_checker(dir.path(), &owner(46_002), &test_options(10), |_| {
                true
            })
            .await
            .unwrap();

        assert_eq!(prior, Some(1_700_000_000_000));
        assert_eq!(fs::read_to_string(lock_path(dir.path())).unwrap(), "46002");
    }

    #[tokio::test]
    async fn test_crash_recovery_for_recent_dead_pid() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path()).await.unwrap();
        tokio::fs::write(lock_path(dir.path()), "47001")
            .await
            .unwrap();

        let prior = try_acquire_with_process_checker(
            dir.path(),
            &owner(47_002),
            &test_options(DEFAULT_HOLDER_STALE_MS),
            |_| false,
        )
        .await
        .unwrap();

        assert!(prior.unwrap() > 0);
        assert_eq!(fs::read_to_string(lock_path(dir.path())).unwrap(), "47002");
    }
}
