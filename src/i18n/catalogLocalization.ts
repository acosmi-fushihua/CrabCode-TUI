/**
 * 技能 / 官方插件「目录展示」本地化覆盖层（display-only）。
 *
 * 背景：内置技能（src/skills/bundled/*.ts）与官方市场插件（acosmi/CrabCode-Plugin）
 * 的标题 / 介绍都写死为英文。本模块在**不改动技能文件、也不改技能逻辑**的前提下，
 * 按当前 UI 语言把这些**展示用**字符串覆盖为中文：
 *   - locale === 'zh-CN' 且表里有项 → 返回中文覆盖
 *   - 其它情况（en-US、或表里无此 name）→ 原样返回 fallback（英文原文）
 *
 * 仅作用于展示层：TUI 斜杠补全 / /help / /skills / /plugin。**不**覆盖喂给
 * 模型的 Skill 工具列表（src/tools/SkillTool/prompt.ts），
 * 模型仍见英文原文，匹配行为零改动。
 *
 * 为什么独立成模块而不并入 locales/zh-CN.ts：后者受 i18n parity 测试约束
 * （zh/en 键须一一对应），而本表是「按技能/插件 name、字段可选」的不同形状；
 * 单独成模块避免污染 parity 测试，也无需为每条造 en 镜像项。本模块只引轻量的
 * ./config.js（getLocale），
 * 不引 118KB 的 locale 大表，以保持 worker 包精简。
 */

import { getLocale } from './config.js'

/** 一条目录项的中文覆盖；title / description 均可选（缺省则回退英文原文）。 */
export type CatalogZhEntry = {
  /** 展示用中文标题（不影响斜杠调用 token / 技能 name）。 */
  title?: string
  /** 展示用中文介绍。 */
  description?: string
}

/**
 * 内置技能 + 官方插件携带技能的中文覆盖，键 = 技能 `name`（稳定调用名）。
 * 未列出的技能（含未来第三方）回退英文原文，不报错。
 */
export const SKILL_CATALOG_ZH: Record<string, CatalogZhEntry> = {
  batch: {
    title: '批量并行改造',
    description:
      '先调研并规划一项大规模改动，再在 5–30 个隔离工作树代理中并行执行，每个代理各开一个 PR。',
  },
  'crabcode-browser': {
    title: '浏览器自动化',
    description:
      '默认的内置浏览器自动化（crabcode-browser）。用 CrabCode 浏览器 CLI 进行导航、快照、截图、控制台日志、网络读取、下载与页面交互。',
  },
  'crabcode-api': {
    title: 'CrabCode API 开发',
    description:
      '用 CrabCode API 或 Acosmi SDK 构建应用。当代码引入 acosmi / @acosmi-ai/sdk / crabcode_agent_sdk，或用户要使用 CrabCode API、Acosmi SDK、Agent SDK 时触发。',
  },
  debug: {
    title: '会话调试',
    description: '为本会话开启调试日志并协助诊断问题。',
  },
  goal: {
    title: '目标攻坚',
    description:
      '把一个目标推进到「验证通过」为止：先定义验收标准，再实施，对抗式自检，反复迭代直到全部通过。适用于「不达标不收工」类需求；快速一次性改动或提问请勿调用。',
  },
  'keybindings-help': {
    title: '快捷键帮助',
    description:
      '当用户想自定义键盘快捷键、重绑按键、添加和弦绑定，或修改 ~/.crabcode/keybindings.json 时使用。例如「重绑 ctrl+s」「添加和弦快捷键」「修改提交键」「自定义快捷键」。',
  },
  loop: {
    title: '定时循环',
    description:
      '按固定间隔重复运行某个提示或斜杠命令（例如 /loop 5m /foo，默认 10 分钟）。',
  },
  'lorem-ipsum': {
    title: '填充文本生成',
    description:
      '为长上下文测试生成填充文本。用参数指定 token 数（如 /lorem-ipsum 50000），输出约等于所请求的 token 数。仅限 Ant。',
  },
  'memory-system': {
    title: '记忆系统指南',
    description:
      '记忆系统（做梦空间）能力地图：数据分层、MemorySearch/MemoryManage 工具用法、做梦与想象周期的配置位与结果反馈渠道。',
  },
  remember: {
    title: '记忆整理',
    description:
      '审阅自动记忆条目并建议晋升到 CRABCODE.md、CRABCODE.local.md 或共享记忆；还会检测各记忆层中过时、冲突与重复的条目。',
  },
  schedule: {
    title: '定时远程代理',
    description: '创建、更新、列出或运行按 cron 计划执行的定时远程代理（触发器）。',
  },
  simplify: {
    title: '代码精简',
    description: '审阅改动过的代码在复用、质量与效率上的问题，并逐一修复。',
  },
  skillify: {
    title: '沉淀为技能',
    description:
      '把本次会话中可复用的流程沉淀成一个技能。在想要捕获的流程结束时调用，可附一句描述。',
  },
  // 每个 userInvocable 内置技能都必须提供 zh-CN 目录项。
  'skill-creator': {
    title: '技能创建器',
    description:
      '用自然语言描述一个流程或能力，通过技能方法论把它做成一个可复用的 CrabCode 技能（SKILL.md）。',
  },
  stuck: {
    title: '卡顿诊断',
    description:
      '[仅 ANT] 排查本机冻结 / 卡死 / 缓慢的 CrabCode 会话，并把诊断报告发到 #crabcode-feedback。',
  },
  'update-config': {
    title: '配置助手',
    description:
      '用此技能通过 settings.json 配置 CrabCode。自动化行为（「以后每当 X」「每次 X」「在 X 前/后」）需要在 settings.json 中配置 hooks（由 harness 执行，而非 CrabCode），记忆/偏好无法替代；也用于权限（「允许 X」）、环境变量（「设置 X=Y」）、hook 排错，或任何对 settings.json / settings.local.json 的改动。',
  },
  verify: {
    title: '改动验证',
    description: '通过实际运行应用来验证一处代码改动是否达成预期。',
  },
  // 官方插件携带的技能（与下方 PLUGIN_CATALOG_ZH 的插件名可能重名）。
  'frontend-design': {
    title: '前端设计',
    description: '创建独具一格、达到生产级质量的前端界面，避免千篇一律的 AI 风格。',
  },
}

/**
 * 市场插件的中文覆盖 —— **已停用（2026-06-13 用户裁决，空表）**。
 *
 * 历史上这里按插件 `name` 硬编码中文 title/description，但 key 与插件实际内容
 * 极易错配（线上现象：内容是「案件深度分析」的插件被覆盖成「中文合同」），且
 * 第三方插件无法维护。现一律改用插件 marketplace.json 自带的 displayName /
 * description（源数据即真源）；第一方插件（含 CrabLaw-CN）若要中文展示，由插件
 * 源仓在其 manifest/marketplace.json 自带中文字段，不在 CrabCode 端硬覆盖。
 *
 * 表保持**空对象** + `localizePlugin*` 函数保留（命中不到 → 恒回退 fallback），
 * 这样 DiscoverPlugins / ManagePlugins 等 caller 无需改动即自动降级到插件源数据。
 */
export const PLUGIN_CATALOG_ZH: Record<string, CatalogZhEntry> = {}

/**
 * 一条工作流目录项的中文覆盖。字段均可选（缺省回退英文原文）。
 *
 * 刻意**不含** `whenToUse`：它只出现在两处模型面（WorkflowTool 的工具目录、
 * `Command.whenToUse` 经 getSlashCommandToolSkills 进模型），没有任何用户可见
 * 渲染点。译了它就是永不被读的死数据 —— 与 SKILL_CATALOG_ZH 只覆盖
 * title/description、模型面保留英文原文的取舍同源。
 */
export type WorkflowCatalogZhEntry = {
  /** 展示用中文介绍（斜杠命令菜单 / 工作流详情）。 */
  description?: string
  /**
   * 阶段覆盖，键 = `meta.phases[].title` 的 **canonical（英文）** 标题。
   *
   * 用 canonical 标题而不是下标做键：`phase("Scope")` 与 `meta.phases[]` 的匹配
   * 走的就是这个字符串，键与匹配路径同源，重排阶段不会让覆盖错位。
   */
  phases?: Record<string, { title?: string; detail?: string }>
}

/**
 * 内置工作流（src/tools/WorkflowTool/bundled/*.ts）的中文覆盖，键 = 工作流 `name`。
 *
 * 与 SKILL_CATALOG_ZH 同构、同理由：脚本源文本里的 `meta` 是 canonical 英文
 * （脚本运行在 vm 沙箱里，且 `phase()` 用标题做匹配键，必须语言无关），中文只在
 * 展示边界覆盖。插件工作流的 name 恒为 `<plugin>:<name>`，永远命中不到本表 →
 * 自动回退插件自带的 meta，无需为第三方维护条目。
 *
 * `catalogLocalization-coverage.test.ts` 会枚举全部已注册内置工作流，逐条断言
 * description 与每个阶段标题在 zh-CN 下都有覆盖 —— 新增内置工作流漏加条目即
 * red，不会像历史上的 /goal 那样静默只发英文。
 */
export const WORKFLOW_CATALOG_ZH: Record<string, WorkflowCatalogZhEntry> = {
  'deep-research': {
    description:
      '深度研究工作流：多角度并行检索、抓取来源、对抗式验证声明、产出带引用的研究报告。',
    phases: {
      Scope: { title: '范围拆解', detail: '把研究问题拆成 5 个互补的检索角度' },
      Search: { title: '检索', detail: '每个角度一个并行检索代理' },
      Fetch: { title: '取源', detail: 'URL 去重后抓取来源并抽取可证伪声明' },
      Verify: {
        title: '验证',
        detail: '每条声明 3 票对抗式验证（2 票证伪即淘汰）',
      },
      Synthesize: {
        title: '综合',
        detail: '合并语义重复、按置信度排序、附来源引用',
      },
    },
  },
}

function isZh(): boolean {
  return getLocale() === 'zh-CN'
}

/** 技能标题：zh 命中表里 title 时返回中文，否则返回 fallback（默认 undefined）。 */
export function localizeSkillTitle(
  name: string,
  fallback?: string,
): string | undefined {
  if (isZh()) {
    const zh = SKILL_CATALOG_ZH[name]?.title
    if (zh) return zh
  }
  return fallback
}

/** 技能介绍：zh 命中表里 description 时返回中文，否则返回 fallback（英文原文）。 */
export function localizeSkillDescription(name: string, fallback: string): string {
  if (isZh()) {
    const zh = SKILL_CATALOG_ZH[name]?.description
    if (zh) return zh
  }
  return fallback
}

/** 插件标题：zh 命中表里 title 时返回中文，否则返回 fallback（默认 undefined）。 */
export function localizePluginTitle(
  name: string,
  fallback?: string,
): string | undefined {
  if (isZh()) {
    const zh = PLUGIN_CATALOG_ZH[name]?.title
    if (zh) return zh
  }
  return fallback
}

/** 插件介绍：zh 命中表里 description 时返回中文，否则返回 fallback（英文原文）。 */
export function localizePluginDescription(
  name: string,
  fallback: string,
): string {
  if (isZh()) {
    const zh = PLUGIN_CATALOG_ZH[name]?.description
    if (zh) return zh
  }
  return fallback
}

/** 工作流介绍：zh 命中表里 description 时返回中文，否则回退英文原文。 */
export function localizeWorkflowDescription(
  name: string,
  fallback: string,
): string {
  if (isZh()) {
    const zh = WORKFLOW_CATALOG_ZH[name]?.description
    if (zh) return zh
  }
  return fallback
}

/**
 * 工作流阶段的展示形态。`canonicalTitle` 必须是 `meta.phases[].title` 的原值
 * （也就是脚本里 `phase("...")` 用的那个字符串）—— 它同时是匹配键，调用方不得
 * 先本地化再拿来查表，否则 zh 下第二次查表必然落空。
 *
 * 返回值只用于渲染；阶段下标解析仍走 canonical 标题，见 WorkflowTool.ts。
 */
export function localizeWorkflowPhase(
  name: string,
  canonicalTitle: string,
  fallbackDetail?: string,
): { title: string; detail?: string } {
  if (isZh()) {
    const zh = WORKFLOW_CATALOG_ZH[name]?.phases?.[canonicalTitle]
    if (zh) {
      // 只译了 title 没译 detail 时回退英文 detail，而不是把 detail 抹掉。
      const detail = zh.detail ?? fallbackDetail
      return {
        title: zh.title ?? canonicalTitle,
        ...(detail === undefined ? {} : { detail }),
      }
    }
  }
  return {
    title: canonicalTitle,
    ...(fallbackDetail === undefined ? {} : { detail: fallbackDetail }),
  }
}

/** 一条命令在目录展示时需要的最小形状（不依赖 Command 全类型，worker 侧也能用）。 */
export type LocalizableCommand = {
  /** 稳定调用名，也是两张覆盖表的键。 */
  name: string
  /** `'workflow'` 走工作流表，其余走技能表。 */
  kind?: string | undefined
  /** 作者写的英文原文，命中不到覆盖项时按原样返回。 */
  description?: string | undefined
}

/**
 * 命令目录描述的**唯一分派点**（W-COMMAND-CATALOG-DISPLAY-UNIFY，2026-07-28）。
 *
 * 为什么必须唯一：技能表与工作流表是两个**独立命名空间**（插件工作流的
 * `<plugin>:<name>` 不得与技能 name 撞车），所以「查哪张表」是一个必须做的判断。
 * 这个判断此前在 5 个展示消费者里各内联一份，结果两处各错一半 —— 一处只译工作流
 * 漏了技能会让斜杠菜单技能描述恒为英文，一处只译技能又会漏掉工作流。
 * 判断本身没有所有者，就没有任何闸门能发现漏改。
 *
 * 本函数只负责**选表**；后缀装饰（`(workflow)` / `(plugin)` / `(bundled)`）仍归调用方，
 * 因为不同展示面的装饰口径本就不同（斜杠菜单用 tag chip 而非后缀）。
 *
 * 反漂移：`tests/unit/catalogLocalization-dispatch-unify.test.ts` 断言除本模块外
 * 无人再直调两个底层 localize*Description。
 */
export function localizeCommandDescription(cmd: LocalizableCommand): string {
  const fallback = cmd.description ?? ''
  return cmd.kind === 'workflow'
    ? localizeWorkflowDescription(cmd.name, fallback)
    : localizeSkillDescription(cmd.name, fallback)
}
