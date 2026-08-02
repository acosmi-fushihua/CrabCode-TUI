import type { ContentBlockParam } from '../../types/api-types.js'
import { registerBundledSkill } from '../bundledSkills.js'

// 内置「技能创建器」(skill-creator)，全局可用。
// 设计要点：
//   - **自包含方法论**：本 skill 的 getPromptForCommand 返回**完整**造技能引导（分析需求 →
//     AskUserQuestion 访谈 → 写 SKILL.md → 确认保存），**不依赖 `skillify`**。原因（偏差 1）：
//     `skillify` 有 `USER_TYPE !== 'ant'` 门控，生产环境普通用户拿不到，无法充当「执行层」。
//   - **CrabCode 格式**（偏差 2）：SKILL.md frontmatter 用 CrabCode 体系
//     （name/description/allowed-tools/when_to_use/argument-hint/arguments/context），**不抄**
//     CrabClaw(Go 运行时) 的 tree_id/能力树绑定/crabclaw.json 格式（CrabCode 加载器不识别）。
//   - **引导式输入**：以用户的自然语言需求描述（slash args）为起点，而非回顾当前会话
//     （skillify 是「capture this session」的回顾式；skill-creator 是「描述需求→造技能」的引导式）。
//   - **全局无门控**：registerBundledSkill 无 USER_TYPE 守卫；esbuild 自动打包进 worker bundle
//     （worker 经 getBundledSkills() lazy import 捡到），无需改打包配置。

const SKILL_CREATOR_PROMPT = `# 创建技能 (Skill Creator){{userDescriptionBlock}}

你正在帮用户把一个可重复的流程 / 能力沉淀成一个可复用的 CrabCode 技能（一个 SKILL.md 文件，
之后 CrabCode 可以通过 \`/<技能名>\` 调用，或在合适场景自动触发）。

## 你的任务

### 第 1 步：理解需求

先分析用户想自动化什么（基于上面的描述；若描述为空或含糊，**第一轮就用 AskUserQuestion 问清**）：
- 想沉淀的可重复流程 / 能力是什么
- 输入 / 参数可能有哪些
- 有哪些清晰的步骤（按顺序）
- 每步的成功标准（不是「写了代码」，而是「PR 已开且 CI 全绿」这种可验证的产物 / 判据）
- 需要哪些工具和权限
- 有哪些硬约束（必须 / 绝不）

### 第 2 步：访谈用户

用 AskUserQuestion 提**所有**问题（绝不用纯文本提问）。每轮迭代到用户满意为止。用户永远有自由文本
「Other」选项，不要自己加「需要微调」之类选项，只给实质性选择。

**第 1 轮 · 高层确认**
- 基于分析建议一个技能名（kebab-case）和一句话描述，请用户确认或改名。
- 建议高层目标 + 具体成功标准。

**第 2 轮 · 细节**
- 把识别出的高层步骤列成有序清单，告诉用户下一轮再深入每步细节。
- 若技能需要参数，基于观察建议参数（说清调用者需要提供什么）。
- 若不明确，问该技能应 **inline**（在当前会话内运行，便于用户中途引导）还是 **fork**
  （作为有独立上下文的子代理运行，适合不需中途介入的自包含任务）。
- 问技能保存位置，按上下文给默认建议：
  - **本仓库**（\`.crabcode/skills/<名>/SKILL.md\`）—— 仅本项目的工作流
  - **个人**（\`~/.crabcode/skills/<名>/SKILL.md\`）—— 跨所有仓库跟随你
  - 仅上述两个位置会被自动扫描并出现在侧栏；若用户选择其它路径，需手动到该工作流所在项目下才能看到。

**第 3 轮 · 拆解每步**
对每个主要步骤，若不显而易见则问：
- 这步产出什么是后续步骤需要的？（数据 / 产物 / ID）
- 什么证明这步成功了、可以往下走？
- 执行前是否需要用户确认？（尤其是合并、发消息、删除等不可逆操作）
- 哪些步骤相互独立、可并行？
- 该步如何执行？（直接做 / 用 Task 子代理 / 用 Teammate 并行协作 / [human] 用户来做）
- 有哪些硬约束或强偏好？必须 / 绝不发生什么？
> 步骤超过 3 个或澄清问题较多时，可多轮 AskUserQuestion（一步一轮）。简单流程别过度提问。

**第 4 轮 · 收尾**
- 确认何时应调用此技能，建议 / 确认触发短语（如「整理今天」「做财务对账」）。
- 问还有什么坑或注意事项。

### 第 3 步：写 SKILL.md（CrabCode 格式）

按下面格式创建技能目录与文件（用户在第 2 轮选的位置）：

\`\`\`markdown
---
name: {{技能名}}
description: {{一句话描述}}
allowed-tools:
  {{会话中观察到 / 需要的工具权限模式，用 Bash(gh:*) 这种精确模式，不要裸 Bash}}
when_to_use: {{何时应自动调用此技能：以「Use when...」开头 + 触发短语 + 示例用户消息}}
argument-hint: "{{参数占位提示}}"
arguments:
  {{参数名列表}}
context: {{inline 或 fork —— inline 可省略}}
---

# {{技能标题}}
技能说明。

## 输入
- \`$参数名\`: 这个输入的说明

## 目标
清晰陈述该工作流的目标，最好有明确的完成产物 / 判据。

## 步骤

### 1. 步骤名
这步做什么，具体可执行，必要时含命令。

**成功标准**：每步**必填**！表明该步完成、可以往下走，可以是清单。
\`\`\`

**逐步注解**（按需）：
- **成功标准** 每步必填。
- **执行方式**：\`直接\`（默认）/ \`Task 子代理\` / \`Teammate\`（真并行 + 代理间通信）/ \`[human]\`（用户做）。非默认才标。
- **产物**：本步产出、后续步骤依赖的数据（PR 号 / commit SHA 等）。仅当后续依赖才写。
- **人工检查点**：何时暂停问用户（不可逆操作 / 错误判断 / 输出审阅）。
- **规则**：工作流硬规则。

**Frontmatter 规则**：
- \`allowed-tools\`：最小权限（用 \`Bash(gh:*)\` 不用裸 \`Bash\`）。
- \`context\`：仅自包含、不需中途介入的技能设 \`context: fork\`。
- \`when_to_use\` 至关重要 —— 告诉模型何时自动调用，以「Use when...」开头并含触发短语。
- \`arguments\` / \`argument-hint\`：仅当技能带参数时写，正文用 \`$名\` 替换。

### 第 4 步：确认并保存

写文件前，先把完整 SKILL.md 作为 yaml 代码块输出在回复里供用户审阅（语法高亮）。然后用
AskUserQuestion 问一句简短确认（如「这个 SKILL.md 可以保存吗？」，别用 body 字段）。

写入后告诉用户：
- 技能保存在哪
- 如何调用：\`/{{技能名}} [参数]\`
- 可直接编辑 SKILL.md 来微调
`

export function registerSkillCreatorSkill(): void {
  registerBundledSkill({
    name: 'skill-creator',
    description:
      '用自然语言描述一个流程或能力，通过技能方法论把它做成一个可复用的 CrabCode 技能（SKILL.md）。',
    aliases: ['create-skill'],
    whenToUse:
      'Use when the user wants to create a new reusable skill from a natural-language description ' +
      'of a workflow or capability. Examples: "创建一个技能", "把这个流程做成技能", "make a skill for ...".',
    allowedTools: [
      'Read',
      'Write',
      'Edit',
      'Glob',
      'Grep',
      'AskUserQuestion',
      'Bash(mkdir:*)',
    ],
    userInvocable: true,
    argumentHint: '[用自然语言描述你想自动化的流程或能力]',
    async getPromptForCommand(args: string): Promise<ContentBlockParam[]> {
      const desc = typeof args === 'string' ? args.trim() : ''
      const userDescriptionBlock = desc
        ? `\n\n用户这样描述他想要的能力：「${desc}」`
        : ''
      const prompt = SKILL_CREATOR_PROMPT.replace(
        '{{userDescriptionBlock}}',
        userDescriptionBlock,
      )
      return [{ type: 'text', text: prompt }]
    },
  })
}
