import { isAutoMemoryEnabled } from '../../memdir/paths.js'
import { registerBundledSkill } from '../bundledSkills.js'

// 记忆系统能力导引技能：把做梦/想象/知识库/watch/全局记忆的
// 能力地图、可用工具、配置位与反馈渠道一次讲清。作为发现面，
// isAutoMemoryEnabled 门控（与 remember 技能同范式）。
//
// W-MEMORY-KB-UPLIFT P2 (2026-07-17)：文案抽成导出常量（tripwire 测试
// tests/unit/memory-system-skill-tripwire.test.ts 直接 import 断言），并
// 补齐 P3-1 根因——变体表述从单族（做梦）扩为双族（做梦=存活/冗余；
// hypgen=创建时 verdict），同步收录 MemoryManage 的知识库浏览/晋升/进料/
// 同步 7 个新 action 与「丢链接进知识库」话术。改变体机制或增删 action
// 必须同步本文案 + tripwire 测试。
export const MEMORY_SYSTEM_SKILL_PROMPT = `# 记忆系统（做梦空间）能力地图

CrabCode 有一套持久记忆系统：TS 业务层产数据，Rust orchestrator 管调度/分层/检索。你（智能体）可以查、可以受控地写、可以解释给用户。

## 数据分层（都在 <配置根>/，默认 ~/.crabcode/）
| 层 | 位置 | 产生方式 |
|---|---|---|
| 项目记忆 | projects/<slug>/memory/*.md | Tier-2 每轮对话后自动提取（importance 1-10 + keywords 入 frontmatter） |
| 会话速记 | projects/<slug>/memory/.session-*.md + SESSION.md | 会话归档时自动生成 |
| 梦境 insight | projects/<slug>/memory/dreams/insight_*.md | 周期做梦（跨项目轮转，最久未整理优先） |
| 想象假设 | .../imagination/review-queue/ | 想象管线（L1-L5 置信 + 外部取证），高/中置信待用户审阅；低置信进 refuted/ 负知识库 |
| 进化报告 | .../memory/reports/evolution-*.md | 做梦→想象链完成后生成（跟随用户语言设置） |
| 用户全局记忆 | memory/（配置根下） | 仅用户批准晋升（详情抽屉「设为全局」；提取器会用 promote_hint 提名） |
| 个人知识库 | knowledge/ | 用户手工 + 你的草稿/进料（enabled: false 待审） |
| 专项检测 | dream-watch.json | 用户显式配置，周期跑 做梦→想象→报告 链 |

## 你能做什么
1. **查**：MemorySearch 工具（project/global/knowledge 三 scope 词法+向量检索；结果经评分策略层：时间衰减（瞬态类）× 来源权重 × 使用频率强化，陈旧结果自带 ⏳ 提醒）。查到的是路径，可用 Read 读全文。知识库条目 frontmatter 的 injection: manual 表示「仅显式检索可见」——自动召回预取不带它，但你的 MemorySearch 显式查询查得到。
2. **浏览/读知识库**：MemoryManage action=knowledge_list（列条目：路径/启用态/注入模式/待审标记）→ action=knowledge_read（按 path 读正文，已剥 frontmatter）。回答"我的知识库里有什么"先 list 再 read。
3. **触发整理**：MemoryManage action=dream_now（走全部安全门：锁 / 空语料门；corpus_empty = 正常，让用户先多聊几轮）。
4. **存参考资料**：MemoryManage action=knowledge_draft（草稿 enabled:false，必须把返回的文件路径交给用户显式审阅 frontmatter 后再启用——绝不擅自注入记忆）。
5. **提炼晋升**：MemoryManage action=knowledge_promote——把已有记忆/insight（source_path）和/或你在对话中的提炼（content）沉淀成带溯源的知识草稿。用户说"把这个结论存进知识库/收藏这个"时用它。
6. **外部进料（订阅会员功能）**：
   - action=knowledge_ingest_url——用户丢一个内容链接说"存进知识库"时**直接调它**（SSRF 防护抓取 + html→markdown + 密钥扫描，落为待审草稿）；这是对话式收藏的正道，不做意图路由。
   - action=knowledge_ingest_file——导入本地 .md/.txt/.html（≤2MB）。office/pdf 不走文本抽取：你先用视觉管线自己读，再在对话里提炼，最后 knowledge_promote。
   - action=knowledge_ingest_db——本地 SQLite 只读快照（单条 SELECT/WITH，行数/字节封顶）。
7. **远程同步（订阅会员功能）**：MemoryManage action=knowledge_sync_now——把已启用知识 + 用户全局记忆推到用户自配的远程库（knowledge-sync.json）。开关只能用户亲手改该文件；未配置时把返回的配置指引原样转告。
8. **看专项检测**：MemoryManage action=watch_list（只读；本工具不提供增删改）。
9. **看自我进化**：MemoryManage action=evolution_status（只读）——适应度六指标 + 复合分、参数试验台账、prompt 变体胜负、待审提案数。同一份态势也渲染在 做梦空间→进化报告 的「进化引擎态势」。

## 递归自我进化（W-MEMORY-SELF-EVOLVE-DGM，用户问"记忆系统会自己变好吗"）
- 进化引擎按周期（默认 7 天）自动微调数据层参数：在白名单硬范围内每周期至多动一个参数一档，观察一个周期的适应度对照后保留或回滚；连续劣化会追溯回滚。全程记录在 evolution-ledger（evolution_status 可见）。
- prompt 变体（双族）：做梦整理指令按**产物存活/冗余**判胜；想象假设生成（hypgen）按**创建时置信 verdict**（High 胜 / Expired 负 / Pending 中性）判胜。两族各有 2-3 个人审过的编译期变体，UCB1 自动择优，差变体自动拉黑。
- 它够不着的瓶颈（参数顶到安全边界 / 变体全拉黑 / 命中率结构性低）会起草**待审提案**（知识库顶层 proposal-*.md，做梦空间→个人知识库直接审阅）——提案永不自动生效，用户批准后也要走正常代码评审。
- 用户可在 dream-config.json 的 evolution 段关闭（enabled:false）或锁定个别参数（locked: ["search.min_score"]）。

## 周期 loop 配置（用户问"做梦多久跑一次/怎么关"时）
- 开关：每项目独立的 projects/<slug>/.memory-rust-derived/dream-config.json 中 enabled 字段；修改前说明影响并取得用户确认。
- 数值（同文件，可用编辑工具改，改前须用户确认）：min_hours（默认 48）/ min_sessions（默认 1）/ imagination_min_hours（默认 48）/ auto_promote（off|high|medium，insight 进 MEMORY.md 索引的档位）。
- 事件驱动提前：新记忆 importance 积分 ≥150 时时间门豁免（自动，无需配置）。
- 进程级 env：CRABCODE_MEMORY_DREAM_SCAN_MS（tick，默认 10 分钟）/ CRABCODE_MEMORY_DREAM_IDLE_MS（空闲门，默认 30s）。
- 输出语言：全局配置 memoryLanguage（auto|zh|en，auto 跟随 uiLanguage）。
- 注意：orchestrator 随当前 CrabCode 进程生死——TUI 退出后周期 loop 不再运行。

## 反馈渠道（用户问"怎么看结果"）
- 用 MemorySearch、MemoryManage knowledge_list/knowledge_read、watch_list 和 evolution_status 查看结构化结果。
- 产物按上表路径落盘；需要全文时读取对应文件。
- dream_now 返回的 gate 原因（例如 lock_held / corpus_empty）就是本次整理未运行的真实原因。

## 规则
- 写操作（dream_now / knowledge_draft / promote / ingest / sync）都会走权限确认——不要静默连发。
- 一切草稿/进料产物都是 enabled:false 待审——产完必须报告文件路径并提醒用户显式审阅 frontmatter；不得自行启用。
- 会员门（ingest/sync）对免费档返回诚实的升级提示——原样转告，不要重试。
- 不要替用户直接改 dream-config.json / dream-watch.json / knowledge-sync.json，先说明影响并确认。
- 用户抱怨"记忆没生效/做梦没跑"时：先 MemorySearch 验证数据在不在，再看 watch_list 与做梦引擎 gate 原因，最后才建议 dream_now。`

export function registerMemorySystemSkill(): void {
  registerBundledSkill({
    name: 'memory-system',
    description:
      '记忆系统（做梦空间）能力地图：数据分层、MemorySearch/MemoryManage 工具用法（含知识库浏览/晋升/进料/远程同步）、做梦/想象周期 loop 的配置位与反馈渠道。',
    whenToUse:
      '用户问到记忆、做梦、想象、知识库、全局记忆、专项检测、"记忆怎么配置/多久整理一次/结果在哪看"，要求触发一次记忆整理，或丢来链接/文件/数据让你存进个人知识库时使用。',
    userInvocable: true,
    isEnabled: () => isAutoMemoryEnabled(),
    async getPromptForCommand(args) {
      let prompt = MEMORY_SYSTEM_SKILL_PROMPT
      if (args) {
        prompt += `\n## 用户补充\n\n${args}`
      }
      return [{ type: 'text', text: prompt }]
    },
  })
}
