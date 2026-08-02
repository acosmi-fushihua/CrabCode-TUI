import { feature } from '../../utils/featurePolyfill.js'
import { logForDebugging } from '../../utils/debug.js'
import { registerBrowserAutomationSkill } from './browserAutomation.js'
import { registerBatchSkill } from './batch.js'
import { registerDebugSkill } from './debug.js'
import { registerKeybindingsSkill } from './keybindings.js'
import { registerLoremIpsumSkill } from './loremIpsum.js'
import { registerMemorySystemSkill } from './memorySystem.js'
import { registerRememberSkill } from './remember.js'
import { registerSimplifySkill } from './simplify.js'
import { registerSkillCreatorSkill } from './skill-creator.js'
import { registerSkillifySkill } from './skillify.js'
import { registerStuckSkill } from './stuck.js'
import { registerUpdateConfigSkill } from './updateConfig.js'
import { registerVerifySkill } from './verify.js'

/**
 * Bundled registration ledger
 * ---------------------------
 * Unconditional bundled skills (registered every startup, see body below):
 *   updateConfig, keybindings, verify, debug, loremIpsum, skillify, remember,
 *   simplify, batch, browserAutomation, stuck
 *
 * Flag-gated optional bundled skills — module / register fn / flag / shipped:
 *   loop.js                  registerLoopSkill                LOCAL_AUTOMATIONS       shipped
 *   scheduleRemoteAgents.js  registerScheduleRemoteAgentsSkill AGENT_TRIGGERS_REMOTE   shipped
 *   crabcodeApi.js           registerCrabCodeApiSkill          BUILDING_CRABCODE_APPS  shipped
 *   goal.js                  registerGoalSkill                 VERIFICATION_AGENT      shipped
 *
 * Dream is implemented as Rust memory-orchestrator policy rather than a
 * TypeScript skill. Tier-1/2/3 在 Rust
 *   orchestrator（`libs/acosmi-memory/acosmi-memory-orchestrator`）实现
 *   为 policy（不是 skill），LLM 调用通过反向 IPC `memory/tier/llmCallRequest`
 *   广播给 TS 跑 SDK。dream subagent agent identity 见 `src/services/
 *   dreamSubagent/agentDefinition.ts`（agentType = "dream-subagent"）。
 */

/**
 * Wrap an optional register call so a single failing skill cannot drag down
 * the rest of bundled registration. This is depth-in-defense against future
 * single-point regressions — not the H1 fix itself (the fork stubs were
 * removed outright). On error the skill is simply skipped; remaining skills
 * still register.
 */
function safeRegister(skillName: string, register: () => void): void {
  try {
    register()
  } catch (err) {
    logForDebugging(
      `[bundled-skills] failed to register optional skill "${skillName}": ${
        err instanceof Error ? err.stack ?? err.message : String(err)
      }`,
      { level: 'error' },
    )
  }
}

/**
 * Initialize all bundled skills.
 * Called at startup to register skills that ship with the CLI.
 *
 * To add a new bundled skill:
 * 1. Create a new file in src/skills/bundled/ (e.g., myskill.ts)
 * 2. Export a register function that calls registerBundledSkill()
 * 3. Import and call that function here
 */
export function initBundledSkills(): void {
  registerUpdateConfigSkill()
  registerKeybindingsSkill()
  registerVerifySkill()
  registerDebugSkill()
  registerLoremIpsumSkill()
  // skill-creator 自包含且全局可用，不受 USER_TYPE 门控；区别于 ant-only 的 skillify。
  registerSkillCreatorSkill()
  registerSkillifySkill()
  registerRememberSkill()
  // W-MEMORY-SYNERGY W7 (2026-07-16) — 记忆系统能力导引（做梦空间地图 +
  // MemorySearch/MemoryManage 用法 + loop 配置位）。运行时按
  // isAutoMemoryEnabled 门控（remember 同范式）。
  registerMemorySystemSkill()
  registerSimplifySkill()
  registerBatchSkill()
  registerBrowserAutomationSkill()
  registerStuckSkill()
  if (feature('LOCAL_AUTOMATIONS')) {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const { registerLoopSkill } = require('./loop.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    // /loop's isEnabled delegates to isKairosCronEnabled() — same lazy
    // per-invocation pattern as the cron tools. Registered unconditionally;
    // the skill's own isEnabled callback decides visibility.
    safeRegister('loop', registerLoopSkill)
  }
  if (feature('AGENT_TRIGGERS_REMOTE')) {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const {
      registerScheduleRemoteAgentsSkill,
    } = require('./scheduleRemoteAgents.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    safeRegister('scheduleRemoteAgents', registerScheduleRemoteAgentsSkill)
  }
  if (feature('BUILDING_CRABCODE_APPS')) {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const { registerCrabCodeApiSkill } = require('./crabcodeApi.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    safeRegister('crabcodeApi', registerCrabCodeApiSkill)
  }
  if (feature('VERIFICATION_AGENT')) {
    /* eslint-disable @typescript-eslint/no-require-imports */
    const { registerGoalSkill } = require('./goal.js')
    /* eslint-enable @typescript-eslint/no-require-imports */
    // /goal depends on the built-in 'verification' agent, which registers
    // under the same flag (builtInAgents.ts); its own isEnabled hides it
    // in coordinator sessions.
    safeRegister('goal', registerGoalSkill)
  }
}
