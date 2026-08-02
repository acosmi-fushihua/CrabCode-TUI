//! W-CRON-DIR-UNIFY PR-1：守护端一次性布局归一。
//!
//! 把 cron 守护的落盘存储从归一前的「双脑」嵌套 `<state>/.crabcode/*` 搬迁到归一
//! 后的单层 `<state>/cron/*`。**文件名全部不变**，只改父目录前缀，因此迁移是近乎
//! 纯粹的目录搬迁（同卷 `rename`，最小面）。
//!
//! ## 何时跑
//!
//! 由 `run_locked_daemon` 在**任何嵌套层写入之前**调用（`initialize_ledger_epoch`
//! = 第一个嵌套层写，之前调用），此时主锁 `cron/scheduled_tasks.lock` 与——当
//! `.crabcode/` 已存在时——旧锁 `.crabcode/scheduled_tasks.lock` 均已持有，故迁移
//! 期间不会有并发旧代守护在写 `.crabcode/`。
//!
//! ## 迁移期重入门（W-CRON-MIGRATION-REENTRY-GUARD，2026-07-20）
//!
//! 上面「均已持有」此前仅是**注释断言**。判活层残余的四重罕见同时命中（活对端 × 其
//! `start_time` 恰对后来者不可读 × 并发双启动 × `.crabcode/` 过渡目录尚在）可让第二个
//! 守护经判活误放行抢到主锁，令两个进程并发进入破坏性迁移、并发 `rename` 同一批数据。
//! 本模块因此在**每一次破坏性 `rename` 之前**调 `acosmi_scheduler::owns_primary_lock`
//! 复核主锁仍归本进程实例（`identity` + `start_time` 正向确认）——不符即 fail-loud 让出
//! （`?` 冒泡 → `main` 的 owned `cleanup_runtime_files` 释放双锁并重启 → 幂等重入自愈），
//! 由持锁胜者独占迁移。这把「至多一个执行者」从注释断言升级为**运行时强制**：从概率上
//! 极罕见变为构造上不可能。触发面随 legacy-first 路径退役一并消失。
//!
//! ## 安全不变量
//!
//! - **纯新装不物化 `.crabcode/`**：`old` 目录不存在即早退，绝不创建它，保持单层
//!   洁净。
//! - **业务锁留旧位**：`scheduled_tasks.lock` 不搬（双锁过渡靠它让旧代守护互斥）。
//! - **零静默丢失**：目标已存在（冲突）时保留 `cron/` 侧权威，把 `.crabcode/` 侧
//!   改名为带时间戳的 `.migration-conflict-<epoch_ms>.bak` 取证留档。
//! - **未知条目留原地**：不在已知族里的条目一律 warn 记名、留在 `.crabcode/`。
//! - **fail-loud**：跨设备（EXDEV / Windows `ERROR_NOT_SAME_DEVICE`）或重试耗尽即
//!   返 `Err` → 冷启动失败 → launcher `ensure_running` 重启 → 幂等重入自愈。
//!
//! ## 墓碑
//!
//! 迁移后 `.crabcode/` 仅留 `MIGRATED` 哨兵 + 过渡期旧锁作自文档墓碑，待
//! `HOLD_LEGACY_LOCK` 退役后清理。

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::fs;

/// 归一前的旧嵌套目录名（相对 `<state>`）。main.rs boot 的 legacy-first 锁判据也
/// 引用它作单一真源。
pub(crate) const LEGACY_DIR_NAME: &str = ".crabcode";

/// 归一后的单层目录名（相对 `<state>`）。
const UNIFIED_DIR_NAME: &str = "cron";

/// 迁移哨兵文件名（写在 `.crabcode/` 内）。
const MIGRATED_SENTINEL: &str = "MIGRATED";

/// 业务锁文件名——**永不搬迁**（双锁过渡靠它）。
const BUSINESS_LOCK_NAME: &str = "scheduled_tasks.lock";

/// 冲突取证 bak 的中缀（`<name>.migration-conflict-<epoch_ms>.bak`）。
const CONFLICT_BAK_INFIX: &str = ".migration-conflict-";

/// `rename` 重试节奏（扛 Windows AV / 索引器的瞬时占用）。3 次重试。
const RENAME_RETRY_DELAYS_MS: [u64; 3] = [50, 250, 1000];

#[cfg(windows)]
const ERROR_NOT_SAME_DEVICE: i32 = 17;

/// 迁移结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayoutMigrationOutcome {
    /// `old` 不存在（纯新装或已退役）——未创建 `new`，未搬任何数据。
    NothingToMigrate,
    /// 已跑过一次搬迁。计数用于日志/哨兵取证。
    Migrated {
        moved: usize,
        conflicts: usize,
        unknown_left: usize,
    },
}

/// 一个 `.crabcode/` 条目的处置分类。
enum Family {
    /// 留旧位（业务锁 / 哨兵 / 历史冲突 bak）。
    Skip,
    /// 搬到 `cron/`（守护数据族）。
    Move,
    /// 不识别——留旧位并 warn。
    Unknown,
}

/// 按**文件名族**分类一个条目。判序严格（skip 守卫必须先于 move 族）：一个形如
/// `scheduled_tasks.json.migration-conflict-N.bak` 的历史冲突 bak 若不先被
/// `.migration-conflict-` 守卫拦下，就会错配进 `scheduled_tasks.json.` move 族。
fn classify(name: &str) -> Family {
    // 1) 业务锁：过渡期留旧位，双锁靠它。
    if name == BUSINESS_LOCK_NAME {
        return Family::Skip;
    }
    // 2) 哨兵。
    if name == MIGRATED_SENTINEL {
        return Family::Skip;
    }
    // 3) 历史冲突取证 bak（必须先于下方 move 族）。
    if name.contains(CONFLICT_BAK_INFIX) {
        return Family::Skip;
    }
    // 4) scheduled_tasks.json 本体 + `.tmp` / `.bak`（`.lock` 已在 1) 排除）。
    if name == "scheduled_tasks.json" || name.starts_with("scheduled_tasks.json.") {
        return Family::Move;
    }
    // 5) outbox 家族：jsonl / ledger_epoch / ledger_started / 轮转 `.1/.2/.3` /
    //    epoch 原子写临时 `<...>.tmp`。
    if name.starts_with("cron_outbox.") {
        return Family::Move;
    }
    // 6) fire 日志 + 轮转。
    if name == "cron_fired.log" || name.starts_with("cron_fired.log.") {
        return Family::Move;
    }
    // 7) ledger db 本体 + `-wal` / `-shm` / `.bak`。
    if name.starts_with("cron_ledger.db") {
        return Family::Move;
    }
    // 8) delivery 存储整目录。
    if name == "cron_delivery_v3" {
        return Family::Move;
    }
    Family::Unknown
}

/// 归一入口：若 `<state>/.crabcode/` 存在则一次性搬迁到 `<state>/cron/`。
///
/// `identity` = 本进程获取主锁时写入的身份（`crabcode-cron-<pid>`）。破坏性 `rename`
/// 前用它复核主锁仍归本进程（W-CRON-MIGRATION-REENTRY-GUARD）。
pub(crate) async fn migrate_legacy_layout_if_present(
    state_dir: &Path,
    identity: &str,
) -> Result<LayoutMigrationOutcome> {
    let old = state_dir.join(LEGACY_DIR_NAME);
    let new = state_dir.join(UNIFIED_DIR_NAME);

    // 1) old 不存在或非目录 → 无事可做，且**不创建** new（纯新装保持单层）。
    match fs::symlink_metadata(&old).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            tracing::warn!(
                path = %old.display(),
                "legacy cron path 不是目录；跳过布局迁移"
            );
            return Ok(LayoutMigrationOutcome::NothingToMigrate);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LayoutMigrationOutcome::NothingToMigrate);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("stat legacy cron dir {}", old.display()));
        }
    }

    // 2) 兜底建 new（主锁通常已建过 `cron/`；幂等）。
    fs::create_dir_all(&new)
        .await
        .with_context(|| format!("create unified cron dir {}", new.display()))?;

    // 3) 先把条目名收集完，避免搬迁（尤其同目录冲突 bak 改名）扰动目录迭代。
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    let mut rd = fs::read_dir(&old)
        .await
        .with_context(|| format!("read legacy cron dir {}", old.display()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .with_context(|| format!("iterate legacy cron dir {}", old.display()))?
    {
        names.push(entry.file_name());
    }
    drop(rd);

    let mut moved = 0usize;
    let mut conflicts = 0usize;
    let mut unknown_left = 0usize;

    for name_os in &names {
        let Some(name) = name_os.to_str() else {
            // 非 UTF-8 文件名：不识别，留旧位。
            tracing::warn!(
                path = %old.join(name_os).display(),
                "legacy cron 条目名非 UTF-8；留在原地（未迁移）"
            );
            unknown_left += 1;
            continue;
        };

        match classify(name) {
            Family::Skip => {}
            Family::Unknown => {
                tracing::warn!(entry = name, "未知 legacy cron 条目；留在原地（未迁移）");
                unknown_left += 1;
            }
            Family::Move => {
                // W-CRON-MIGRATION-REENTRY-GUARD：每一次破坏性 rename 前复核主锁仍归本
                // 进程实例——被并发守护回收/易主即中止让出，绝不与持锁胜者并发写同一迁移。
                ensure_still_primary_owner(state_dir, identity).await?;
                let src = old.join(name);
                let dst = new.join(name);
                if path_exists(&dst).await {
                    // 冲突：保留 new 侧权威，把 old 侧改名为带时间戳的取证 bak（同目录
                    // → 同卷，永不 EXDEV），零静默丢失。
                    let epoch_ms = chrono::Utc::now().timestamp_millis();
                    let bak = old.join(format!("{name}{CONFLICT_BAK_INFIX}{epoch_ms}.bak"));
                    rename_with_retry(&src, &bak).await.with_context(|| {
                        format!(
                            "quarantine conflicting legacy {} → {}",
                            src.display(),
                            bak.display()
                        )
                    })?;
                    conflicts += 1;
                    tracing::warn!(
                        entry = name,
                        bak = %bak.display(),
                        "cron 布局迁移冲突：保留 cron/ 侧权威，隔离 legacy 侧取证"
                    );
                } else {
                    rename_with_retry(&src, &dst).await.with_context(|| {
                        format!("migrate legacy cron {} → {}", src.display(), dst.display())
                    })?;
                    moved += 1;
                }
            }
        }
    }

    write_sentinel(&old, &new, moved, conflicts, unknown_left).await?;

    Ok(LayoutMigrationOutcome::Migrated {
        moved,
        conflicts,
        unknown_left,
    })
}

/// W-CRON-MIGRATION-REENTRY-GUARD：破坏性 `rename` 前复核本进程仍持主锁。
///
/// 主锁 `cron/scheduled_tasks.lock` 被并发守护回收/易主（`session_id` 或 `start_time`
/// 不符），或锁已消失，即返回 `Err` 中止迁移——**不 `rename`、不写任何数据**。`?` 冒泡到
/// `main`，其 owned `cleanup_runtime_files` 释放双锁且从不发布 PID，launcher
/// `ensure_running` 重启 → 幂等重入自愈；迁移交由持锁胜者独占。可用优先失败方向：让出，
/// 不双写。
async fn ensure_still_primary_owner(state_dir: &Path, identity: &str) -> Result<()> {
    if !acosmi_scheduler::owns_primary_lock(state_dir, identity).await {
        anyhow::bail!(
            "cron 主锁已被回收/易主（W-CRON-MIGRATION-REENTRY-GUARD）——\
             中止布局迁移让出给持锁胜者，未执行破坏性 rename"
        );
    }
    Ok(())
}

/// `path` 是否存在（含符号链接本身；lstat 语义，不跟随）。
async fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).await.is_ok()
}

/// `fs::rename` 带重试。跨设备错误立即 fail-loud（同卷不变量被破坏，重试无益）。
async fn rename_with_retry(src: &Path, dst: &Path) -> Result<()> {
    let mut attempt = 0usize;
    loop {
        match fs::rename(src, dst).await {
            Ok(()) => return Ok(()),
            Err(error) if is_cross_device(&error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cross-device rename {} → {}——legacy 与 unified cron 目录必须同卷",
                        src.display(),
                        dst.display()
                    )
                });
            }
            Err(error) => {
                if attempt >= RENAME_RETRY_DELAYS_MS.len() {
                    return Err(error).with_context(|| {
                        format!(
                            "rename {} → {} 重试耗尽仍失败",
                            src.display(),
                            dst.display()
                        )
                    });
                }
                tokio::time::sleep(Duration::from_millis(RENAME_RETRY_DELAYS_MS[attempt])).await;
                attempt += 1;
            }
        }
    }
}

/// 判定一个 IO 错误是否为跨设备（同卷不变量违反）。
fn is_cross_device(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

/// 写/刷新 `.crabcode/MIGRATED` 哨兵墓碑。
async fn write_sentinel(
    old: &Path,
    new: &Path,
    moved: usize,
    conflicts: usize,
    unknown_left: usize,
) -> Result<()> {
    let sentinel = old.join(MIGRATED_SENTINEL);
    let body = format!(
        "cron 布局已归一到: {}\n\
         migrated_at: {}\n\
         moved: {moved}\n\
         conflicts: {conflicts}\n\
         unknown_left: {unknown_left}\n\
         数据已归一到 cron/，此目录仅留过渡期 legacy 锁与本哨兵，待 HOLD_LEGACY_LOCK 退役后清理。\n",
        new.display(),
        chrono::Utc::now().to_rfc3339(),
    );
    fs::write(&sentinel, body)
        .await
        .with_context(|| format!("write migration sentinel {}", sentinel.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// 迁移测试统一 identity；命中 Move 臂的用例先以此获取主锁，令复核门放行
    /// （W-CRON-MIGRATION-REENTRY-GUARD）。不命中 Move 臂的用例复核门不触发，identity
    /// 值无关紧要。
    const TEST_OWNER: &str = "migration-test-owner";

    fn write_file(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write test file");
    }

    #[tokio::test]
    async fn nothing_to_migrate_when_old_dir_absent() {
        let state = tempdir().expect("tempdir");
        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(out, LayoutMigrationOutcome::NothingToMigrate);
        // 纯新装：绝不从迁移里创建 cron/，也不物化 .crabcode/。
        assert!(
            !state.path().join("cron").exists(),
            "纯新装迁移不得创建 cron/"
        );
        assert!(!state.path().join(".crabcode").exists());
    }

    #[tokio::test]
    async fn moves_known_families_preserving_filenames() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(
            &old.join("scheduled_tasks.json"),
            br#"{"version":2,"jobs":[]}"#,
        );
        write_file(&old.join("cron_outbox.jsonl"), b"line\n");
        write_file(&old.join("cron_outbox.ledger_epoch"), b"epoch\n");
        write_file(&old.join("cron_fired.log"), b"fired\n");
        write_file(&old.join("cron_ledger.db"), b"db");
        let deliv = old.join("cron_delivery_v3");
        std::fs::create_dir_all(&deliv).expect("mk deliv");
        write_file(&deliv.join("evt-1.jsonl"), b"evt");

        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), TEST_OWNER)
                .await
                .expect("acquire primary lock")
        );

        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(
            out,
            LayoutMigrationOutcome::Migrated {
                moved: 6,
                conflicts: 0,
                unknown_left: 0,
            }
        );

        let new = state.path().join("cron");
        for f in [
            "scheduled_tasks.json",
            "cron_outbox.jsonl",
            "cron_outbox.ledger_epoch",
            "cron_fired.log",
            "cron_ledger.db",
        ] {
            assert!(new.join(f).exists(), "{f} 必须搬到 cron/");
            assert!(!old.join(f).exists(), "{f} 必须从 .crabcode/ 消失");
        }
        assert!(
            new.join("cron_delivery_v3").join("evt-1.jsonl").exists(),
            "delivery 整目录（含子文件）必须搬迁"
        );
        assert!(!old.join("cron_delivery_v3").exists());
        assert!(old.join("MIGRATED").exists(), "哨兵必须写入");
    }

    #[tokio::test]
    async fn never_moves_business_lock() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("scheduled_tasks.lock"), b"{}");

        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(
            out,
            LayoutMigrationOutcome::Migrated {
                moved: 0,
                conflicts: 0,
                unknown_left: 0,
            }
        );
        // 负测：业务锁必须留旧位（双锁过渡关键）。
        assert!(
            old.join("scheduled_tasks.lock").exists(),
            "业务锁必须留在 .crabcode/（勿搬）"
        );
        assert!(
            !state
                .path()
                .join("cron")
                .join("scheduled_tasks.lock")
                .exists(),
            "业务锁绝不搬到 cron/"
        );
    }

    #[tokio::test]
    async fn leaves_unknown_and_warns() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("random.txt"), b"?");

        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(
            out,
            LayoutMigrationOutcome::Migrated {
                moved: 0,
                conflicts: 0,
                unknown_left: 1,
            }
        );
        assert!(old.join("random.txt").exists(), "未知条目留旧位");
        assert!(!state.path().join("cron").join("random.txt").exists());
    }

    #[tokio::test]
    async fn conflict_keeps_new_baks_old() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        let new = state.path().join("cron");
        std::fs::create_dir_all(&old).expect("mk old");
        std::fs::create_dir_all(&new).expect("mk new");
        write_file(&old.join("scheduled_tasks.json"), b"OLD");
        write_file(&new.join("scheduled_tasks.json"), b"NEW");

        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), TEST_OWNER)
                .await
                .expect("acquire primary lock")
        );

        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(
            out,
            LayoutMigrationOutcome::Migrated {
                moved: 0,
                conflicts: 1,
                unknown_left: 0,
            }
        );

        // new 侧权威、内容不变。
        assert_eq!(
            std::fs::read(new.join("scheduled_tasks.json")).expect("read new"),
            b"NEW"
        );
        // old 侧被隔离成 conflict bak，内容 = 旧字节。
        let baks: Vec<String> = std::fs::read_dir(&old)
            .expect("read old")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.starts_with("scheduled_tasks.json.migration-conflict-") && n.ends_with(".bak")
            })
            .collect();
        assert_eq!(baks.len(), 1, "恰一个 conflict bak");
        assert_eq!(
            std::fs::read(old.join(&baks[0])).expect("read bak"),
            b"OLD",
            "conflict bak 必须保全旧字节"
        );
        assert!(
            !old.join("scheduled_tasks.json").exists(),
            "old 侧原名已改为 bak"
        );
    }

    #[tokio::test]
    async fn idempotent_reentry() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("scheduled_tasks.json"), b"J");
        write_file(&old.join("cron_outbox.jsonl"), b"O");

        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), TEST_OWNER)
                .await
                .expect("acquire primary lock")
        );

        let first = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("first");
        assert_eq!(
            first,
            LayoutMigrationOutcome::Migrated {
                moved: 2,
                conflicts: 0,
                unknown_left: 0,
            }
        );

        let new = state.path().join("cron");
        let before = std::fs::read(new.join("scheduled_tasks.json")).expect("read new");

        let second = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("second");
        assert_eq!(
            second,
            LayoutMigrationOutcome::Migrated {
                moved: 0,
                conflicts: 0,
                unknown_left: 0,
            },
            "第二次重入稳定无搬迁"
        );
        assert_eq!(
            std::fs::read(new.join("scheduled_tasks.json")).expect("read new again"),
            before,
            "已搬数据在重入后不变"
        );
        let bak_count = std::fs::read_dir(&old)
            .expect("read old")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".migration-conflict-")
            })
            .count();
        assert_eq!(bak_count, 0, "幂等重入不得产生 conflict bak");
    }

    #[tokio::test]
    async fn sentinel_references_target() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("scheduled_tasks.json"), b"J");

        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), TEST_OWNER)
                .await
                .expect("acquire primary lock")
        );

        migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        let body = std::fs::read_to_string(old.join("MIGRATED")).expect("read sentinel");
        assert!(body.contains("cron"), "哨兵必须引用归一目标 cron/：{body}");
    }

    #[tokio::test]
    async fn rotations_and_sidecars_move() {
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("cron_fired.log.1"), b"r");
        write_file(&old.join("cron_ledger.db-wal"), b"w");
        write_file(&old.join("cron_ledger.db-shm"), b"s");
        write_file(&old.join("cron_outbox.ledger_started"), b"started\n");

        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), TEST_OWNER)
                .await
                .expect("acquire primary lock")
        );

        let out = migrate_legacy_layout_if_present(state.path(), TEST_OWNER)
            .await
            .expect("migrate");
        assert_eq!(
            out,
            LayoutMigrationOutcome::Migrated {
                moved: 4,
                conflicts: 0,
                unknown_left: 0,
            }
        );
        let new = state.path().join("cron");
        for f in [
            "cron_fired.log.1",
            "cron_ledger.db-wal",
            "cron_ledger.db-shm",
            "cron_outbox.ledger_started",
        ] {
            assert!(new.join(f).exists(), "{f} 轮转/sidecar 必须搬迁");
            assert!(!old.join(f).exists());
        }
    }

    #[tokio::test]
    async fn reentry_guard_loser_aborts_before_rename_winner_migrates() {
        // W-CRON-MIGRATION-REENTRY-GUARD 验收：模拟判活层残余让第二个守护抢到主锁——终局
        // 是主锁归 "winner"（最后写锁者赢）。原持有者 "loser"（其锁已被 winner 回收/覆盖）
        // 进入迁移时，复核门读到主锁 session_id="winner" ≠ "loser" → 中止让出（Err），
        // **未执行任何破坏性 rename**（数据仍在 .crabcode/、未进 cron/、无 conflict bak）。
        // 随后 winner 迁移成功。断言「至多一个进入 rename」。
        //
        // 跨平台（非 #[cfg(windows)]）：复核门本身跨平台（读锁 + 比对身份），事故的判活误
        // 放行虽是 Windows 特有，但重入门语义与之正交，全平台生效、全平台可测；当前 Windows
        // 宿主亦覆盖。
        let state = tempdir().expect("tempdir");
        let old = state.path().join(".crabcode");
        std::fs::create_dir_all(&old).expect("mk old");
        write_file(&old.join("scheduled_tasks.json"), b"J");
        write_file(&old.join("cron_outbox.jsonl"), b"O");

        // winner 抢到并持有主锁（= 判活误放行后的终局）。
        assert!(
            acosmi_scheduler::try_acquire_lock(state.path(), "winner")
                .await
                .expect("winner lock")
        );

        // loser（其主锁已被 winner 覆盖）进入迁移 → 复核门在第一次 rename 前中止让出。
        let loser = migrate_legacy_layout_if_present(state.path(), "loser").await;
        assert!(
            loser.is_err(),
            "loser 的主锁已易主，迁移必须在复核门中止（Err），而非执行搬迁"
        );

        // 关键不变量：loser **未执行任何破坏性 rename**——数据仍在 .crabcode/，未进 cron/。
        let new = state.path().join("cron");
        assert!(
            old.join("scheduled_tasks.json").exists(),
            "loser 中止前不得搬走 scheduled_tasks.json"
        );
        assert!(
            !new.join("scheduled_tasks.json").exists(),
            "loser 绝不能把数据搬进 cron/"
        );
        assert!(
            old.join("cron_outbox.jsonl").exists() && !new.join("cron_outbox.jsonl").exists(),
            "loser 中止前不得搬走任何条目"
        );
        // 无 conflict bak（未误隔离，无双写痕迹）。
        let baks = std::fs::read_dir(&old)
            .expect("read old")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".migration-conflict-")
            })
            .count();
        assert_eq!(baks, 0, "loser 中止不得产生 conflict bak（无双写/误隔离）");

        // winner 持锁 → 复核门放行、正常搬迁全部条目。
        let won = migrate_legacy_layout_if_present(state.path(), "winner")
            .await
            .expect("winner migrate");
        assert_eq!(
            won,
            LayoutMigrationOutcome::Migrated {
                moved: 2,
                conflicts: 0,
                unknown_left: 0,
            },
            "winner 持锁必须放行并完成迁移"
        );
        assert!(new.join("scheduled_tasks.json").exists());
        assert!(new.join("cron_outbox.jsonl").exists());
        assert!(!old.join("scheduled_tasks.json").exists());

        acosmi_scheduler::release_lock(state.path(), "winner")
            .await
            .expect("release");
    }
}
