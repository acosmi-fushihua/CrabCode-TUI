//! 跨进程调度器锁（兼容 TS cronTasksLock.ts）
//!
//! 使用 `O_EXCL` 原子创建 `cron/scheduled_tasks.lock`（主锁；W-CRON-DIR-UNIFY
//! 归一后单层）。过渡期另持旧锁 `.crabcode/scheduled_tasks.lock`（见
//! `LEGACY_LOCK_FILE_REL` / `HOLD_LEGACY_LOCK`），让仍在跑的旧代守护也能互斥。
//! 格式: `{ "sessionId": "...", "pid": N, "acquiredAt": N }`
//!
//! 协议与 TS 侧 `tryAcquireSchedulerLock` 完全兼容：
//! - 文件存在且 PID 存活 → 被占用
//! - 文件存在且 PID 已死 → 陈旧锁，回收后重试
//! - 文件不存在 → 原子创建

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing;

// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const LOCK_FILE_REL: &str = "cron/scheduled_tasks.lock";

/// 过渡期旧业务锁相对路径（归一前形态）。
///
/// W-CRON-DIR-UNIFY 双锁过渡：归一后主锁走 `cron/scheduled_tasks.lock`，但一个
/// 仍在跑的**旧代**守护（PR-1 之前的二进制）只认这个 `.crabcode/` 旧锁、不认新
/// 主锁。过渡期新守护 boot 时**先**竞争此旧锁（仅当 `.crabcode/` 目录已存在），
/// 撞见旧代守护就退避，未动任何旧数据即退出。
///
/// 退役条件：`HOLD_LEGACY_LOCK` 至少跨 **2 个已发布版本**（最早 1.0.20+）后方可
/// 退役；退役时须同步删除本常量、`HOLD_LEGACY_LOCK`、main.rs boot 的 legacy-first
/// 获取/释放路径，并更新 tripwire 与兼容性说明。
// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const LEGACY_LOCK_FILE_REL: &str = ".crabcode/scheduled_tasks.lock";

/// 过渡期是否同时持有旧业务锁（见 `LEGACY_LOCK_FILE_REL` 的退役条件）。
pub const HOLD_LEGACY_LOCK: bool = true;

/// 锁文件内容（与 TS `SchedulerLock` 类型对齐）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerLock {
    pub session_id: String,
    pub pid: u32,
    pub acquired_at: i64,
    /// 持锁进程的启动时刻（sysinfo `Process::start_time`，秒级）。
    ///
    /// PID-reuse 防御（2026-06-05）：仅靠 `is_pid_alive(pid)` 在 PID 被回收后会把
    /// 一个**完全无关**的新进程误判成"原持锁进程还活着"，导致陈旧锁永不回收、
    /// daemon 永远 `exit(11)` 起不来。陈旧检查改为"PID 存活**且** start_time 一致"
    /// 才算 live。
    ///
    /// `Option` + `skip_serializing_if`：
    /// - 旧锁文件无此字段 → 反序列化为 `None` → 回退到 bare-PID 检查（向后兼容）；
    /// - 某些平台/权限下取不到 start_time → 写 `None`，同样回退 bare-PID。
    ///
    /// 该锁文件 Rust 独占（无 TS 侧 acquire），serde 对 TS 写出的未知键宽容，故新增
    /// 字段对前向兼容无影响。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
}

pub(crate) fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE_REL)
}

/// 过渡期旧业务锁完整路径（`<dir>/.crabcode/scheduled_tasks.lock`）。
pub(crate) fn legacy_lock_path(dir: &Path) -> PathBuf {
    dir.join(LEGACY_LOCK_FILE_REL)
}

/// 检查 PID 是否存活（跨平台）——**僵尸感知**。
///
/// ROOT-CAUSE FIX (2026-04-23)：旧实现在 Windows 上无条件返回 `false`，把所有
/// 锁都判成"陈旧"并回收 —— 两个并行 scheduler daemon 会同时以为自己持锁，
/// 跨进程锁在 Windows 彻底失效。
///
/// Windows 分支使用 `GetExitCodeProcess`，判活
/// 对齐 launcher `pid_alive`。此前只凭 `OpenProcess` 可否成功判活——一个**已退出
/// 但句柄被其它进程钉住的僵尸进程**（`OpenProcess` 仍成功、`GetExitCodeProcess`
/// 返回真实退出码 ≠ `STILL_ACTIVE`）被误判成"活着"，令新守护在记录着该僵尸 PID 的
/// 陈旧 legacy 锁上永久 `exit(11)` 起不来。
///
/// ## 判活三实现同源契约 —— 改一处必核对三处
///
/// | # | 位置 | 僵尸感知 |
/// |---|---|---|
/// | A | `acosmi-daemon-launcher::pid_alive` (`src/lib.rs`) | ✅ 基准（Win `GetExitCodeProcess` / Unix `kill`+`ESRCH`）|
/// | B | `acosmi-scheduler::lock::is_pid_alive`（**本函数**）| ✅ 本次补齐（曾僵尸盲 → 楔死守护）|
/// | C | `acosmi-exec::tree_kill::is_process_alive` (`src/tree_kill.rs`) | ✅ Windows 已对齐 |
///
/// ## 统一判活真值表（与 launcher `pid_alive` 对齐）
///
/// | 观测 | 存活判定 | 复用防御 |
/// |---|---|---|
/// | `OpenProcess` 成功 + `GetExitCodeProcess == STILL_ACTIVE(259)` | 活 | launcher: `pid_identity`；本层: `start_time` 一致性 |
/// | `OpenProcess` 成功 + 退出码 ≠ 259（僵尸：句柄钉住的已退出进程）| **死** →（回收路径）| — |
/// | `OpenProcess` 失败 `ERROR_INVALID_PARAMETER`(87) | 死（唯一可证不存在的失败码）| — |
/// | `OpenProcess` 失败 其它（含 ACCESS_DENIED）| 存在但无权限 → "存活" | 由身份/复用层判定是否"原持有者" |
///
/// 即「存活」只回答"这个 PID 有活着的进程"；"是不是我们那个实例"交给上层的
/// `start_time` **正向确认门**（见 `try_acquire_lock_at`），不再靠可打开性猜。
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // POSIX: kill(pid, 0) 不发送信号，仅检查进程是否存在
        // SAFETY: signal 0 是存在性检查，不实际发送信号
        #[allow(unsafe_code)]
        // pid u32 → i32 的 wraparound 风险：实际 pid 不超过 PID_MAX (~4 million)，
        // 远低于 i32::MAX；libc::pid_t 是 i32 平台默认。
        #[allow(clippy::cast_possible_wrap)]
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        // W-CRON-LIVENESS-PARITY (2026-07-20) — Unix 僵尸不变量（已核实，故此处
        // 不主动排除 Zombie）：一个**未被 wait 回收**的 Unix 僵尸同样让 kill(pid,0)
        // 返 0（PID 表项仍被占用），是 Windows「句柄钉住僵尸」的同源体。但本仓
        // cron 守护的唯一 Unix spawn 路径是 acosmi-daemon-launcher::spawn_unix::
        // daemonize 的 double-fork + waitpid（见该文件对中间子进程的 waitpid）——真
        // 守护是 grandchild、被 init 收养并由 init reap，中间子进程被父 waitpid 立即
        // 回收 → **本仓不产生能持有本锁的 Unix 僵尸**；reap 后 kill(pid,0)=ESRCH 正确
        // 判死（这正是 mac 免疫本次事故的机理）。**若未来引入非 double-fork 的 Unix
        // 守护 spawn 路径**（长命父进程直接 Command::spawn 且不 reap），须在此补：
        //   `alive && !matches!(process_status(pid), Some(sysinfo::ProcessStatus::Zombie))`
        // （sysinfo 本 crate 已依赖、零新增；取不到状态时保守判活以不误删合法锁），
        // 并加 #[cfg(unix)] fork-僵尸回归测试，过独立 mac 回归门。见判活四实现同源契约。
        alive
    }
    #[cfg(windows)]
    {
        // SAFETY: OpenProcess + GetExitCodeProcess + CloseHandle 都是标准 Win32 API，
        // 无内存安全副作用。
        #[allow(unsafe_code)]
        unsafe {
            let handle = windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0, // bInheritHandle = FALSE
                pid,
            );
            if handle.is_null() {
                // 区分"明确不存在"（ERROR_INVALID_PARAMETER=87）与"无权限"
                // （ERROR_ACCESS_DENIED=5 等）。只有前者才真正代表进程已死；其余
                // 保守判活（与 Unix kill(pid,0) 的 ESRCH 语义、launcher pid_alive 对齐）。
                let err = windows_sys::Win32::Foundation::GetLastError();
                const ERROR_INVALID_PARAMETER: u32 = 87;
                err != ERROR_INVALID_PARAMETER
            } else {
                // 句柄可打开 ≠ 进程活着：一个已退出但被其它进程句柄钉住的**僵尸**
                // 同样能 OpenProcess 成功（本次事故 PID 14628 即此形态）。必须再查退出
                // 码——只有 STILL_ACTIVE(259) 才是真活；其它退出码 = 已退出僵尸 → 判死。
                let mut code: u32 = 0;
                let got =
                    windows_sys::Win32::System::Threading::GetExitCodeProcess(handle, &mut code);
                let _ = windows_sys::Win32::Foundation::CloseHandle(handle);
                const STILL_ACTIVE: u32 = 259;
                // GetExitCodeProcess 失败（罕见——QUERY_LIMITED_INFORMATION 已含查询退出
                // 码的权限）时保守判活，避免误删合法锁（此处对锁比 launcher 更保守：
                // launcher 无锁可护，失败即判死）。
                if got != 0 { code == STILL_ACTIVE } else { true }
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // 其它平台无法判定 → 保守假设存活，宁可让锁无法回收也不误判恢复。
        let _ = pid;
        true
    }
}

/// 读取指定 PID 当前进程的启动时刻（秒级），用于 PID-reuse 防御。
///
/// 返回 `None` 表示该 PID 当前无进程，或当前平台/权限下取不到 start_time —— 调用方
/// 应回退到 bare-PID 检查，绝不能据此判定"已死"（否则会误删合法锁）。
fn process_start_time(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    // 只刷新目标 PID、且不取额外信息（start_time 是 refresh 时无条件填充的基础字段），
    // 避免全量进程扫描开销。
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        false,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).map(|p| p.start_time())
}

/// 在**显式锁文件路径**获取调度器锁（主锁 / 旧锁共用内核）。
///
/// 成功返回 true，锁已被其他存活进程持有返回 false。
/// 陈旧锁会被自动回收（PID-reuse 防御见 `SchedulerLock::start_time`）。
async fn try_acquire_lock_at(path: PathBuf, identity: &str) -> std::io::Result<bool> {
    let lock = SchedulerLock {
        session_id: identity.to_owned(),
        pid: std::process::id(),
        acquired_at: chrono::Utc::now().timestamp_millis(),
        // 记录本进程启动时刻，供陈旧检查做 PID-reuse 防御（取不到则 None → 回退 bare-PID）。
        start_time: process_start_time(std::process::id()),
    };
    let body = serde_json::to_string(&lock).map_err(std::io::Error::other)?;

    // 确保目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    // 尝试原子创建（O_EXCL）
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
    {
        Ok(file) => {
            drop(file);
            fs::write(&path, &body).await?;
            tracing::info!(pid = std::process::id(), "获取调度器锁");
            return Ok(true);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 继续检查现有锁
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 目录不存在，已在上面 create_dir_all，重试
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(file) => {
                    drop(file);
                    fs::write(&path, &body).await?;
                    return Ok(true);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }

    // 锁文件已存在，读取并检查
    let existing = fs::read_to_string(&path)
        .await
        .ok()
        .and_then(|content| serde_json::from_str::<SchedulerLock>(&content).ok());

    if let Some(ref existing_lock) = existing {
        // 已经是我们的锁
        if existing_lock.session_id == identity {
            // 更新 PID（进程重启场景）
            if existing_lock.pid != std::process::id() {
                fs::write(&path, &body).await?;
            }
            return Ok(true);
        }

        // 其他进程持有 → **正向确认门**（W-CRON-LIVENESS-PARITY，2026-07-20）。
        //
        // 退避（return Ok(false)，不获取）**当且仅当能正向确认「同一实例仍存活」**：
        // - is_pid_alive(pid) 为真（Fix 1a 后已僵尸感知），**且**
        // - 锁记录了 start_time，**且**
        // - 当前同 PID 进程的 start_time 与记录一致（= 同一实例，非 PID 复用）。
        //
        // 其余一切组合——死 / 僵尸 / start_time 取不到 / start_time 不符 / 锁未记录
        // start_time——都**无法证明同实例存活** → 回收（可用优先）。这翻正了旧门
        // 「证不出死就保留（守护不可用）」的反向失败方向：本次事故里「僵尸可 Open ×
        // start_time 取不到」曾落进保留分支，令新守护永久 exit(11) 起不来。新版锁恒记
        // start_time（见 try_acquire_lock_at），故「未记录 start_time」仅命中旧格式锁，
        // 其持有者若在也是旧代守护，回收后由命名管道 FIRST_PIPE_INSTANCE bind 兜底。
        let alive = is_pid_alive(existing_lock.pid);
        let same_instance_alive = alive
            && match existing_lock.start_time {
                Some(recorded) => process_start_time(existing_lock.pid) == Some(recorded),
                None => false,
            };

        if same_instance_alive {
            tracing::debug!(
                held_by = %existing_lock.session_id,
                pid = existing_lock.pid,
                "调度器锁被其他进程持有（同实例存活，正向确认）"
            );
            return Ok(false);
        }

        // 无法正向确认同实例存活 → 回收（记录判据便于诊断）。
        tracing::info!(
            stale_pid = existing_lock.pid,
            alive,
            had_recorded_start_time = existing_lock.start_time.is_some(),
            "回收陈旧调度器锁（无法正向确认同实例存活）"
        );
    }

    // 回收：删除后重试原子创建（一次机会）
    let _ = fs::remove_file(&path).await;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
    {
        Ok(file) => {
            drop(file);
            fs::write(&path, &body).await?;
            tracing::info!(pid = std::process::id(), "回收后获取调度器锁");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 其他进程赢得了竞争
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// 尝试获取调度器主锁（`cron/scheduled_tasks.lock`）。
pub async fn try_acquire_lock(dir: &Path, identity: &str) -> std::io::Result<bool> {
    try_acquire_lock_at(lock_path(dir), identity).await
}

/// 复核主锁当前是否仍归**本进程实例**所有（W-CRON-MIGRATION-REENTRY-GUARD，2026-07-20）。
///
/// 迁移期重入门（`layout_migration`）用它在**每一次破坏性 `rename` 之前**确认主锁
/// `cron/scheduled_tasks.lock` 没被并发守护回收/易主。返回 `true` 当且仅当磁盘上的主锁：
/// - 存在且可解析，**且**
/// - `session_id == identity`（本进程获取锁时写入的身份；identity 内含本进程 PID，故本
///   进程存活期间没有第二个进程能持有同一 identity 字符串——这一条即足以判「仍是我们」），
///   **且**
/// - `start_time == process_start_time(self)`（同实例正向确认 · PID-reuse 纵深；本进程存活
///   时 PID 不会被复用，此项是冗余防御，与获取路径的 `SchedulerLock::start_time` 对偶）。
///
/// 其余一切（锁不存在 / 不可解析 / 被他人回收覆盖致 session_id 不符 / start_time 不符）
/// 一律返回 `false` —— 调用方应中止破坏性操作并让出（可用优先：宁可让持锁胜者独占迁移，
/// 也不容两个进程并发写同一迁移）。本函数纯只读，不改任何锁文件。
///
/// 为何锚定**主锁**而非过渡期旧锁：主锁 `try_acquire_lock` 是跨启动周期的唯一 owner 真源
/// （O_EXCL + 陈旧回收 + start_time 正向确认门）；旧锁只是让 PR-1 之前的旧代守护也能互斥的
/// 过渡兜底，退役后即消失。迁移「至多一个执行者」的裁决必须以主锁为准。
pub async fn owns_primary_lock(dir: &Path, identity: &str) -> bool {
    let path = lock_path(dir);
    let Ok(content) = fs::read_to_string(&path).await else {
        return false;
    };
    let Ok(existing) = serde_json::from_str::<SchedulerLock>(&content) else {
        return false;
    };
    existing.session_id == identity && existing.start_time == process_start_time(std::process::id())
}

/// 过渡期：尝试获取旧业务锁（`.crabcode/scheduled_tasks.lock`）。撞见仍在跑的旧代
/// 守护即返回 false（调用方退避），未动任何旧数据。
pub async fn try_acquire_legacy_lock(dir: &Path, identity: &str) -> std::io::Result<bool> {
    try_acquire_lock_at(legacy_lock_path(dir), identity).await
}

/// 释放**显式锁文件路径**的调度器锁（仅当我们持有时；主锁 / 旧锁共用内核）。
async fn release_lock_at(path: PathBuf, identity: &str) -> std::io::Result<()> {
    let content = match fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    if let Ok(existing) = serde_json::from_str::<SchedulerLock>(&content)
        && existing.session_id != identity
    {
        return Ok(()); // 不是我们的锁
    }

    let _ = fs::remove_file(&path).await;
    tracing::info!("释放调度器锁");
    Ok(())
}

/// 释放调度器主锁（`cron/scheduled_tasks.lock`）。
pub async fn release_lock(dir: &Path, identity: &str) -> std::io::Result<()> {
    release_lock_at(lock_path(dir), identity).await
}

/// 过渡期：释放旧业务锁（`.crabcode/scheduled_tasks.lock`）。幂等；纯新装从未创建
/// 该锁 → NotFound no-op，且绝不创建 `.crabcode/`。
pub async fn release_legacy_lock(dir: &Path, identity: &str) -> std::io::Result<()> {
    release_lock_at(legacy_lock_path(dir), identity).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_and_release() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let acquired = try_acquire_lock(dir.path(), "test-session")
            .await
            .expect("lock");
        assert!(acquired);

        // 重复获取同一 identity 应成功（幂等）
        let again = try_acquire_lock(dir.path(), "test-session")
            .await
            .expect("lock");
        assert!(again);

        release_lock(dir.path(), "test-session")
            .await
            .expect("release");
        assert!(!lock_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn blocked_by_live_process() {
        let dir = tempfile::tempdir().expect("tmpdir");

        // 先获取锁
        let acquired = try_acquire_lock(dir.path(), "owner").await.expect("lock");
        assert!(acquired);

        // 不同 identity 应被阻塞（同一 PID 在测试中是存活的）
        let blocked = try_acquire_lock(dir.path(), "other").await.expect("lock");
        assert!(!blocked);

        release_lock(dir.path(), "owner").await.expect("release");
    }

    #[tokio::test]
    async fn pid_reuse_with_mismatched_start_time_is_stale_and_reacquirable() {
        // PID-reuse 防御回归：磁盘锁记录了当前存活 PID，但 start_time 是一个
        // 不可能匹配的值（模拟原持锁进程已退出、PID 被本进程"复用"）。
        // 旧实现仅 is_pid_alive → 误判仍持有 → 永远阻塞；新实现应判陈旧并允许回收。
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = lock_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");

        let stale = SchedulerLock {
            session_id: "ghost-owner".to_owned(),
            pid: std::process::id(), // 存活 PID（本进程）
            acquired_at: 0,
            start_time: Some(1), // 与本进程真实 start_time 必然不符
        };
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).expect("write stale lock");

        // 不同 identity 应能回收陈旧锁并获取。
        let acquired = try_acquire_lock(dir.path(), "new-owner")
            .await
            .expect("lock");
        assert!(
            acquired,
            "start_time 不符的复用 PID 锁必须被判陈旧并可重新获取"
        );

        // 锁现在归 new-owner，且带本进程真实 start_time。
        let content = std::fs::read_to_string(&path).expect("read lock");
        let now: SchedulerLock = serde_json::from_str(&content).expect("parse");
        assert_eq!(now.session_id, "new-owner");
        assert_eq!(now.pid, std::process::id());
        assert!(
            now.start_time.is_some(),
            "重新获取的锁应记录本进程真实 start_time"
        );

        release_lock(dir.path(), "new-owner")
            .await
            .expect("release");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn legacy_lock_reclaimed_when_holder_is_windows_zombie() {
        // 事故根治回归（W-CRON-LIVENESS-PARITY）：造一个真实 Windows 僵尸——spawn 一个
        // 立即退出的子进程，wait 到它退出，但**保持 Child 句柄不 drop** → 进程对象被
        // 句柄钉住，OpenProcess(pid) 仍成功，但 GetExitCodeProcess 返回真实退出码 ≠
        // STILL_ACTIVE。这正是事故里 PID 14628 的形态。Fix 1a 前 is_pid_alive 误判其
        // "活"、令陈旧锁永不回收（守护 exit(11)）；Fix 1a 后判死，正向确认门回收之。
        use std::process::{Command, Stdio};

        let mut child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zombie child");
        let zombie_pid = child.id();
        // 等它退出；Child 句柄仍被持有 → 进程对象钉住成僵尸（PID 不被复用）。
        child.wait().expect("wait zombie child to exit");

        // 判活必须判死（GetExitCodeProcess ≠ 259），尽管 OpenProcess 仍可成功。
        assert!(
            !is_pid_alive(zombie_pid),
            "已退出的僵尸进程必须判死（OpenProcess 成功但退出码 ≠ STILL_ACTIVE）"
        );

        // 写一个记录该僵尸 PID 的陈旧锁（带 start_time，复刻事故 recorded=Some 形态）。
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = lock_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let stale = SchedulerLock {
            session_id: "zombie-owner".to_owned(),
            pid: zombie_pid,
            acquired_at: 0,
            start_time: Some(1_784_186_806),
        };
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).expect("write stale lock");

        // 新 identity 必须能回收僵尸锁并获取（否则守护 exit(11) 永不起来）。
        let acquired = try_acquire_lock(dir.path(), "new-owner")
            .await
            .expect("lock");
        assert!(acquired, "僵尸 PID 持有的陈旧锁必须被回收（事故根治）");

        // 显式保持 child 句柄存活到断言之后再释放（drop 关句柄，让 OS 回收 PID）。
        drop(child);
        release_lock(dir.path(), "new-owner")
            .await
            .expect("release");
    }

    #[tokio::test]
    async fn reclaim_when_lock_has_no_recorded_start_time() {
        // Fix 2 正向确认门 · 外层 None 臂（旧格式锁未记录 start_time）：holder=本进程
        // （存活），但锁无 start_time → 无法正向确认"同一实例" → 必须可回收。旧门此格
        // 走 `None => false`（保留），会让新守护误退避。
        //
        // 注：内层 None 臂（alive 且 recorded=Some 但当前 start_time 取不到）与本臂、与
        // pid_reuse_with_mismatched_start_time 用例走**同一个** `process_start_time(pid)
        // == Some(recorded)` 布尔表达式（取不到 = None ≠ Some(recorded) → 回收），已被
        // 那两个用例覆盖；且"存活却枚举不到 start_time"无法在单测里确定性构造（依赖 OS
        // 保护进程的 sysinfo 行为），故不单列脆弱用例。
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = lock_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let legacy = SchedulerLock {
            session_id: "legacy-format-owner".to_owned(),
            pid: std::process::id(), // 存活（本进程）
            acquired_at: 0,
            start_time: None, // 旧格式：无 start_time 记录
        };
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).expect("write legacy lock");

        let acquired = try_acquire_lock(dir.path(), "new-owner")
            .await
            .expect("lock");
        assert!(
            acquired,
            "无 start_time 记录的锁无法正向确认同实例存活 → 必须可回收"
        );

        // 回收后锁归 new-owner。
        let content = std::fs::read_to_string(&path).expect("read lock");
        let now: SchedulerLock = serde_json::from_str(&content).expect("parse");
        assert_eq!(now.session_id, "new-owner");

        release_lock(dir.path(), "new-owner")
            .await
            .expect("release");
    }

    #[tokio::test]
    async fn owns_primary_lock_true_for_self_false_for_others_and_absent() {
        // W-CRON-MIGRATION-REENTRY-GUARD 复核门原语：
        // - 本进程以 "me" 获取主锁后，owns_primary_lock(dir, "me") == true；
        // - 不同 identity（模拟被并发守护回收/易主）== false；
        // - 无锁文件（被删/从未建）== false。
        let dir = tempfile::tempdir().expect("tmpdir");

        // 锁不存在 → false。
        assert!(
            !owns_primary_lock(dir.path(), "me").await,
            "无主锁文件必须判非本实例所有"
        );

        assert!(try_acquire_lock(dir.path(), "me").await.expect("lock"));
        assert!(
            owns_primary_lock(dir.path(), "me").await,
            "本进程持有的主锁必须判本实例所有"
        );
        assert!(
            !owns_primary_lock(dir.path(), "other").await,
            "identity 不符（他人回收/易主）必须判非本实例所有"
        );

        release_lock(dir.path(), "me").await.expect("release");
        assert!(
            !owns_primary_lock(dir.path(), "me").await,
            "释放后主锁消失，复核必须判 false"
        );
    }

    #[tokio::test]
    async fn release_wrong_identity_noop() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let _ = try_acquire_lock(dir.path(), "real-owner").await;

        // 释放不匹配的 identity 不应删除锁
        release_lock(dir.path(), "imposter").await.expect("release");
        assert!(lock_path(dir.path()).exists());

        // 清理
        release_lock(dir.path(), "real-owner")
            .await
            .expect("release");
    }

    #[tokio::test]
    async fn legacy_lock_is_independent_from_primary() {
        // 双锁过渡核心不变量：主锁与旧锁是两个独立文件，各自独立获取/释放；持有
        // 主锁不阻塞不同 identity 获取旧锁，释放旧锁也不动主锁。
        let dir = tempfile::tempdir().expect("tmpdir");

        assert!(
            try_acquire_lock(dir.path(), "primary-owner")
                .await
                .expect("primary lock")
        );
        assert_eq!(
            lock_path(dir.path()),
            dir.path().join("cron/scheduled_tasks.lock"),
            "主锁必须落在归一后的 cron/ 单层"
        );
        assert!(lock_path(dir.path()).exists());

        // 不同 identity 仍可获取旧锁（独立文件，互不阻塞）。
        assert!(
            try_acquire_legacy_lock(dir.path(), "legacy-owner")
                .await
                .expect("legacy lock")
        );
        assert_eq!(
            legacy_lock_path(dir.path()),
            dir.path().join(".crabcode/scheduled_tasks.lock"),
            "旧锁必须留在 .crabcode/（过渡期形态，勿翻 cron/）"
        );
        assert!(legacy_lock_path(dir.path()).exists());

        // 释放旧锁不影响主锁。
        release_legacy_lock(dir.path(), "legacy-owner")
            .await
            .expect("release legacy");
        assert!(!legacy_lock_path(dir.path()).exists());
        assert!(lock_path(dir.path()).exists(), "释放旧锁不得动主锁");

        release_lock(dir.path(), "primary-owner")
            .await
            .expect("release primary");
        assert!(!lock_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn release_legacy_lock_absent_is_noop_and_creates_no_dir() {
        // 纯新装语义：从未创建旧锁 → release_legacy_lock 是 NotFound no-op，
        // 且绝不物化 .crabcode/。
        let dir = tempfile::tempdir().expect("tmpdir");
        release_legacy_lock(dir.path(), "whoever")
            .await
            .expect("noop release");
        assert!(
            !dir.path().join(".crabcode").exists(),
            "释放不存在的旧锁绝不能创建 .crabcode/"
        );
    }
}
