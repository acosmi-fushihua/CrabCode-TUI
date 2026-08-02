//! W-MEMORY-SELF-EVOLVE-DGM 8a–8d (2026-07-16) — 记忆系统递归自我进化引擎。
//!
//! Darwin Gödel Machine 的**安全收窄适配**：进化对象绝不是生产代码，而是
//! 三类预审计有界对象——①数值参数（`dream-config.json` 白名单键，硬范围
//! 网格内）②prompt 变体**选择权重**（变体本体全部是编译期常量，契约
//! #15-8 不破）③流程开关。"Gödel 递归"（系统提议改进自己的改进机制）只
//! 产出人审草稿（`proposals`），经用户批准 + 正常 PR 管线才进代码。
//!
//! 子模块：
//! - [`gate_stats`] — 做梦 tick 结果计数（8a 门效率指标数据源）；
//! - [`fitness`] — 适应度快照（六指标 + 复合分 + 历史 + 报告投影）；
//! - [`tuner`] — 参数自调优（试验→观察→保留/回滚，ledger 全程可审计）；
//! - [`variants`] — prompt 变体档案（UCB1 选择 + 胜负归因 + 拉黑）；
//! - [`proposals`] — 瓶颈触发的人审提案草稿（永不自动落码）。
//!
//! 所有状态落 `<project_state_dir>/.memory-rust-derived/evolution/`（派生
//! 层：可清可重建，丢失只重置进化进度，绝不 fail-hard）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod fitness;
pub mod gate_stats;
pub mod proposals;
pub mod tuner;
pub mod variants;

/// 进化引擎的派生层根目录。
#[must_use]
pub fn evolution_dir(project_state_dir: &Path) -> PathBuf {
    crate::daily_log::rust_derived_root(project_state_dir).join("evolution")
}

/// 重活（变体 sweep / 适应度 / 调参 / 提案 / 报告）是否该跑：做梦成功
/// （新产物落地）或进化周期到期。包装层用它决定要不要为影子护栏拉起
/// SE（引擎 + 索引 daemon 是重资源，轮转候选每 tick 被门跳过时绝不
/// 白拉）；`on_dream_tick_outcome` 用同一判据早退 —— 单点防漂移。
#[must_use]
pub fn heavy_pass_due(project_state_dir: &Path, outcome_kind: &str, now_ms: u64) -> bool {
    if outcome_kind == "dreamed" {
        return true;
    }
    let config = crate::dream_config::read_dream_config(project_state_dir).unwrap_or_default();
    let state = tuner::load_state(project_state_dir);
    now_ms.saturating_sub(state.last_cycle_ms)
        >= config.evolution.cycle_hours.saturating_mul(3_600_000)
}

/// 进化引擎的**单一挂点**：每次 dream tick 判定后调用（`ipc_handler::
/// dream_one_project` 包装层）。职责：
/// 1. 每 tick：gate 结果计数（廉价）；
/// 2. 有意义时刻（做梦成功落了新产物，或进化周期到期）：变体存活 sweep →
///    适应度快照 + 历史 → 参数试验周期 → 提案检查 → 报告投影。
///
/// 全程 fail-soft —— 进化引擎的任何问题都绝不影响做梦本体。
pub async fn on_dream_tick_outcome(
    base_dir: &Path,
    memory_dir: &Path,
    project_state_dir: &Path,
    now_ms: u64,
    outcome_kind: &str,
    shadow_se: Option<Arc<crate::se_integration::SearchEngineIntegration>>,
    language: crate::output_language::MemoryOutputLanguage,
) {
    // R4-2：这是**自驱周期 tick** lane。手动 `run_now` 与 TS-line runner
    // 回执各自在自己的落点记账（`ipc_handler::spawn_dream_now` /
    // `result_listener::handle_completed`）—— 三条 lane 全记，否则 `dreamed`
    // 系统性漏计成功、`error_rate` 读数是错的。
    gate_stats::record_tick_outcome(
        project_state_dir,
        gate_stats::LANE_TICK,
        outcome_kind,
        now_ms,
    )
    .await;

    if !heavy_pass_due(project_state_dir, outcome_kind, now_ms) {
        return;
    }

    variants::survivor_sweep(memory_dir, project_state_dir, now_ms).await;
    let snapshot = fitness::compute_and_record(memory_dir, project_state_dir, now_ms).await;

    let fetch_owned = shadow_se.map(|se| {
        move |query: &str| -> Vec<crate::se_integration::MemorySearchHit> {
            // include_manual=false：适应度探针度量的是被动召回的命中率，
            // 口径必须与召回一致（manual 条目本就不进被动召回）。
            se.search(query, 10, "text", false).unwrap_or_default()
        }
    });
    let fetch_ref: Option<tuner::ShadowFetch<'_>> = match &fetch_owned {
        Some(fetch) => Some(fetch),
        None => None,
    };

    let tuner_lines =
        tuner::run_evolution_cycle(project_state_dir, now_ms, &snapshot, fetch_ref).await;
    // 提案与试验同门（毁灭复核修正）：用户 evolution.enabled=false 表示
    // 「进化引擎别动」，草稿起草也停；适应度与报告投影保留（纯观测）。
    let evolution_enabled = crate::dream_config::read_dream_config(project_state_dir)
        .map(|cfg| cfg.evolution.enabled)
        .unwrap_or(true);
    let proposal_lines = if evolution_enabled {
        proposals::maybe_draft_proposals(base_dir, project_state_dir, &snapshot, language).await
    } else {
        Vec::new()
    };

    // 报告附加段（表头按语言；行内容来自各模块，保持简洁中文/中英混排
    // 可接受 —— 台账键名本身是英文参数键）。
    let zh = matches!(language, crate::output_language::MemoryOutputLanguage::Zh);
    let mut extra = String::new();
    let push_section = |extra: &mut String, title_zh: &str, title_en: &str, body: &str| {
        if body.trim().is_empty() {
            return;
        }
        extra.push_str("\n## ");
        extra.push_str(if zh { title_zh } else { title_en });
        extra.push_str("\n\n");
        extra.push_str(body);
        extra.push('\n');
    };
    let bullet_lines = |lines: &[String]| -> String {
        lines
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    push_section(
        &mut extra,
        "参数试验",
        "Parameter trials",
        &bullet_lines(&tuner_lines),
    );
    push_section(
        &mut extra,
        "试验台账（尾部）",
        "Trial ledger (tail)",
        &tuner::render_ledger_tail(project_state_dir, 8),
    );
    push_section(
        &mut extra,
        "prompt 变体档案",
        "Prompt variant archive",
        &variants::render_stats(project_state_dir),
    );
    push_section(
        &mut extra,
        "待审提案",
        "Pending proposals",
        &bullet_lines(&proposal_lines),
    );

    fitness::write_fitness_report(memory_dir, &snapshot, &extra, language).await;
}
