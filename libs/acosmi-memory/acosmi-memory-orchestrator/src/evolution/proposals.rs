//! 8d — 自我改写提案（Gödel 递归的安全出口）。
//!
//! 当适应度指向进化器自身够不着的瓶颈时，起草**人审提案草稿**落
//! `<base>/knowledge/proposal-*.md`（`pending_review: true`，复用知识库
//! 草稿的 wx 独占创建纪律；落**顶层**是因为 TUI `knowledge/list` 只扫
//! 顶层文件——子目录对用户不可见 = 没有人审面，毁灭复核修正）。
//! **提案永不自动生效**：用户批准只代表「值得做」，落码仍走正常人写/
//! 人审 PR —— 系统提议改进自己的改进机制，人类与 CI 验证，这是 DGM
//! 递归在闭源生产系统里的唯一合规形态。
//!
//! 触发条件（v1 三类，模板化生成 —— 确定性、零 LLM、可测）：
//! 1. `retrieval-review` — 命中率 < 0.3 且样本 ≥ 20（检索面结构性问题，
//!    参数微调救不了）；
//! 2. `range-extension:<param>` — 参数顶在网格边界且最近两次该参数试验
//!    均 kept 朝边界方向（进化器想走得更远但范围拦住了）；
//! 3. `variant-replacement:<id>` — 变体被拉黑（需要人写新变体替补）。
//!
//! 幂等：同 kind 已有待审提案（`proposal_kind` frontmatter 匹配）→ 跳过。

use std::path::Path;

use crate::evolution::tuner::{LedgerEntry, ParamSpec, PARAM_SPECS};
use crate::evolution::variants::VariantArchive;
use crate::output_language::MemoryOutputLanguage;

/// 提案文件名前缀（知识库顶层与普通条目共存的判别键）。
pub const PROPOSAL_FILE_PREFIX: &str = "proposal-";
/// 命中率提案阈值。
pub const HIT_RATE_PROPOSAL_FLOOR: f64 = 0.3;
pub const HIT_RATE_MIN_SAMPLES: u64 = 20;

fn proposals_dir(base_dir: &Path) -> std::path::PathBuf {
    base_dir.join("knowledge")
}

/// 目录里是否已有同 kind 的待审提案（只读 `proposal-*.md`，不碰普通
/// 知识条目）。
fn kind_already_pending(dir: &Path, kind: &str) -> bool {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(PROPOSAL_FILE_PREFIX) || !name.ends_with(".md") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.contains(&format!("proposal_kind: {kind}"))
            && content.contains("pending_review: true")
        {
            return true;
        }
    }
    false
}

fn kind_slug(kind: &str) -> String {
    kind.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// wx 独占创建（存在则加数字后缀重试，学 MemoryManageTool knowledge_draft
/// 的 TOCTOU 纪律）。fail-soft：写不进去只 warn。
async fn write_proposal(dir: &Path, kind: &str, body: &str) -> Option<std::path::PathBuf> {
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        log::warn!("[evolution] proposals dir create failed (fail-soft): {e}");
        return None;
    }
    let slug = kind_slug(kind);
    for attempt in 0..5u32 {
        let name = if attempt == 0 {
            format!("proposal-{slug}.md")
        } else {
            format!("proposal-{slug}-{attempt}.md")
        };
        let path = dir.join(name);
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                // tokio File 不 flush 就 drop 会丢缓冲写（atomic_write 全链
                // 自带 flush+fsync，这里因 wx 独占创建语义手写，必须补齐）。
                let write = async {
                    file.write_all(body.as_bytes()).await?;
                    file.flush().await?;
                    file.sync_all().await
                };
                if let Err(e) = write.await {
                    log::warn!("[evolution] proposal write failed (fail-soft): {e}");
                    let _ = tokio::fs::remove_file(&path).await;
                    return None;
                }
                return Some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                log::warn!("[evolution] proposal create failed (fail-soft): {e}");
                return None;
            }
        }
    }
    None
}

fn frontmatter(kind: &str, title: &str) -> String {
    format!(
        "---\nname: {title}\nenabled: false\ninjection: manual\npending_review: true\nsource: evolution_engine\nproposal_kind: {kind}\n---\n"
    )
}

/// 参数是否顶边界且最近两次该参数 kept 试验都朝这个边界推。
fn param_pinned_at_edge(spec: &ParamSpec, current: f64, ledger: &[LedgerEntry]) -> bool {
    let index = spec
        .grid
        .iter()
        .position(|g| (g - current).abs() < 1e-9)
        .unwrap_or(usize::MAX);
    let at_low = index == 0;
    let at_high = index == spec.grid.len() - 1;
    if !at_low && !at_high {
        return false;
    }
    let kept: Vec<&LedgerEntry> = ledger
        .iter()
        .filter(|e| e.param == spec.key && e.status == "kept")
        .collect();
    if kept.len() < 2 {
        return false;
    }
    let toward_edge = |e: &LedgerEntry| {
        if at_high {
            e.to > e.from
        } else {
            e.to < e.from
        }
    };
    kept.iter().rev().take(2).all(|e| toward_edge(e))
}

/// 检查触发条件并起草缺失的提案。返回报告行。
pub async fn maybe_draft_proposals(
    base_dir: &Path,
    project_state_dir: &Path,
    snapshot: &crate::evolution::fitness::FitnessSnapshot,
    language: MemoryOutputLanguage,
) -> Vec<String> {
    let dir = proposals_dir(base_dir);
    let ledger = crate::evolution::tuner::read_ledger(project_state_dir);
    let archive: VariantArchive = crate::evolution::variants::load_archive(project_state_dir);
    let config = crate::dream_config::read_dream_config(project_state_dir).unwrap_or_default();
    let zh = matches!(language, MemoryOutputLanguage::Zh);
    let mut lines = Vec::new();

    // 1. 命中率结构性瓶颈。
    if let Some(hit) = snapshot.retrieval_hit_rate {
        if hit < HIT_RATE_PROPOSAL_FLOOR
            && snapshot.retrieval_samples >= HIT_RATE_MIN_SAMPLES
            && !kind_already_pending(&dir, "retrieval-review")
        {
            let title = if zh {
                "提案：检索面结构性复审"
            } else {
                "Proposal: retrieval review"
            };
            let body = if zh {
                format!(
                    "{}# 提案：检索面结构性复审\n\n进化引擎观察到检索命中率 {hit:.2}\
                     （样本 {samples}）持续低于 {floor}，参数网格内的微调已不足以解释 —— \
                     可能是索引覆盖缺口、分词/字段权重结构问题或记忆产物质量问题。\n\n\
                     建议人工检查：索引根覆盖范围、BM25F 字段权重、embedding 可用性、\
                     产物 abstract 质量。\n\n> 本提案由进化引擎模板化生成，批准仅代表\
                     「值得排查」，任何改动走正常评审。\n",
                    frontmatter("retrieval-review", title),
                    hit = hit,
                    samples = snapshot.retrieval_samples,
                    floor = HIT_RATE_PROPOSAL_FLOOR,
                )
            } else {
                format!(
                    "{}# Proposal: retrieval review\n\nThe evolution engine observed a sustained \
                     retrieval hit rate of {hit:.2} ({samples} samples), below {floor}. In-grid \
                     parameter tuning cannot explain this — likely an index-coverage gap, \
                     tokenizer/field-weight structure issue, or artifact quality problem.\n\n\
                     Suggested manual checks: index roots, BM25F field weights, embedding \
                     availability, artifact abstracts.\n\n> Template-generated by the evolution \
                     engine; approval means \"worth investigating\" — any change goes through \
                     normal review.\n",
                    frontmatter("retrieval-review", title),
                    hit = hit,
                    samples = snapshot.retrieval_samples,
                    floor = HIT_RATE_PROPOSAL_FLOOR,
                )
            };
            if write_proposal(&dir, "retrieval-review", &body)
                .await
                .is_some()
            {
                lines.push("已起草提案：retrieval-review".to_string());
            }
        }
    }

    // 2. 参数顶边界。
    for spec in PARAM_SPECS {
        let current = (spec.read)(&config);
        if param_pinned_at_edge(spec, current, &ledger) {
            let kind = format!("range-extension:{}", spec.key);
            if kind_already_pending(&dir, &kind) {
                continue;
            }
            let title = if zh {
                format!("提案：扩展参数范围 {}", spec.key)
            } else {
                format!("Proposal: extend range of {}", spec.key)
            };
            let body = format!(
                "{}# {title}\n\n{}\n\n- {} = {current}\n- grid = {:?}\n\n> {}\n",
                frontmatter(&kind, &title),
                if zh {
                    "该参数最近两次保留的试验都朝网格边界推进且已顶到边界 —— 进化器\
                     认为更远处可能更优，但硬范围（安全钳位）拦住了。请人工评估扩展\
                     网格是否安全（扩展 = 改 `evolution/tuner.rs::PARAM_SPECS` + 固化\
                     测试 + SoT，正常 PR 流程）。"
                } else {
                    "The last two kept trials for this parameter both pushed toward the grid \
                     boundary and it is now pinned there — the tuner believes better values may \
                     lie beyond, but the hard range (safety clamp) stops it. Please evaluate \
                     whether extending the grid is safe (extend = edit \
                     `evolution/tuner.rs::PARAM_SPECS` + pinned tests + SoT, normal PR flow)."
                },
                spec.key,
                spec.grid,
                if zh {
                    "进化引擎模板化生成，永不自动扩范围。"
                } else {
                    "Template-generated; ranges are never extended automatically."
                },
            );
            if write_proposal(&dir, &kind, &body).await.is_some() {
                lines.push(format!("已起草提案：{kind}"));
            }
        }
    }

    // 3. 变体拉黑。
    for (variant_id, stats) in &archive.stats {
        if !stats.blacklisted() {
            continue;
        }
        let kind = format!("variant-replacement:{variant_id}");
        if kind_already_pending(&dir, &kind) {
            continue;
        }
        let title = if zh {
            format!("提案：替补被拉黑的 prompt 变体 {variant_id}")
        } else {
            format!("Proposal: replace blacklisted prompt variant {variant_id}")
        };
        let body = format!(
            "{}# {title}\n\n- {} {}/{}（{:.2}）\n\n{}\n",
            frontmatter(&kind, &title),
            variant_id,
            stats.wins,
            stats.samples(),
            stats.win_rate(),
            if zh {
                "该变体样本充分且胜率低于拉黑线，已退出采样。请人工撰写替补变体\
                 （改 `evolution/variants.rs` 常量族 + 谱系 + tripwire 测试，正常 PR\
                 流程）；在替补落地前，选择器自动回退可用变体/基线。"
            } else {
                "This variant has sufficient samples and a win rate below the blacklist floor; \
                 it is out of sampling. Please author a replacement (edit the constant family in \
                 `evolution/variants.rs` + lineage + tripwire tests via a normal PR). Until \
                 then the selector falls back to remaining variants/baseline."
            },
        );
        if write_proposal(&dir, &kind, &body).await.is_some() {
            lines.push(format!("已起草提案：{kind}"));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::evolution::fitness::FitnessSnapshot;
    use crate::evolution::variants::VariantStats;

    fn low_hit_snapshot() -> FitnessSnapshot {
        FitnessSnapshot {
            retrieval_hit_rate: Some(0.1),
            retrieval_samples: 25,
            ..FitnessSnapshot::default()
        }
    }

    #[tokio::test]
    async fn low_hit_rate_drafts_pending_review_proposal_idempotently() {
        let base = TempDir::new().unwrap();
        let psd = TempDir::new().unwrap();

        let lines = maybe_draft_proposals(
            base.path(),
            psd.path(),
            &low_hit_snapshot(),
            MemoryOutputLanguage::Zh,
        )
        .await;
        assert!(
            lines.iter().any(|l| l.contains("retrieval-review")),
            "{lines:?}"
        );

        // 提案落知识库**顶层**（TUI knowledge/list 只扫顶层 → 有人审面）。
        let dir = proposals_dir(base.path());
        assert_eq!(dir, base.path().join("knowledge"));
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert!(files[0]
            .file_name()
            .to_string_lossy()
            .starts_with(PROPOSAL_FILE_PREFIX));
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(content.contains("pending_review: true"));
        assert!(content.contains("enabled: false"));
        assert!(content.contains("source: evolution_engine"));
        assert!(content.contains("proposal_kind: retrieval-review"));

        // 幂等：同 kind 已待审 → 不再起草。
        let lines = maybe_draft_proposals(
            base.path(),
            psd.path(),
            &low_hit_snapshot(),
            MemoryOutputLanguage::Zh,
        )
        .await;
        assert!(lines.is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn healthy_metrics_draft_nothing() {
        let base = TempDir::new().unwrap();
        let psd = TempDir::new().unwrap();
        let snapshot = FitnessSnapshot {
            retrieval_hit_rate: Some(0.9),
            retrieval_samples: 100,
            ..FitnessSnapshot::default()
        };
        let lines =
            maybe_draft_proposals(base.path(), psd.path(), &snapshot, MemoryOutputLanguage::Zh)
                .await;
        assert!(lines.is_empty());
        assert!(!proposals_dir(base.path()).exists());
    }

    #[tokio::test]
    async fn blacklisted_variant_triggers_replacement_proposal() {
        let base = TempDir::new().unwrap();
        let psd = TempDir::new().unwrap();
        let mut archive = crate::evolution::variants::load_archive(psd.path());
        archive.stats.insert(
            "phase3/v1".to_string(),
            VariantStats {
                wins: 0,
                losses: 15,
                last_used_ms: 1,
            },
        );
        crate::evolution::variants::save_archive(psd.path(), &archive).await;

        let lines = maybe_draft_proposals(
            base.path(),
            psd.path(),
            &FitnessSnapshot::default(),
            MemoryOutputLanguage::En,
        )
        .await;
        assert!(
            lines
                .iter()
                .any(|l| l.contains("variant-replacement:phase3/v1")),
            "{lines:?}"
        );
        let dir = proposals_dir(base.path());
        let content = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect::<String>();
        assert!(content.contains("proposal_kind: variant-replacement:phase3/v1"));
        assert!(content.contains("blacklisted prompt variant"));
    }
}
