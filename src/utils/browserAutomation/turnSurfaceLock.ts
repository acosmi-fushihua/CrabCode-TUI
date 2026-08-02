import type { AutomationTurnSurface } from './turnToolSurface.js'
import {
  automationSurfaceForSkillName,
  classifyAutomationSurfaceIntent,
} from './intentRoute.js'

export type LockedAutomationTurnSurface =
  | 'ordinary'
  | AutomationTurnSurface

export type AutomationTurnSurfaceResolution = {
  automationSurface: AutomationTurnSurface | undefined
  commandAllowRules: string[] | undefined
}

export class AutomationTurnSurfaceTransitionError extends Error {
  readonly code = 'automation_surface_transition_blocked'
  readonly lockedSurface: LockedAutomationTurnSurface
  readonly requestedSurface: AutomationTurnSurface

  constructor(
    lockedSurface: LockedAutomationTurnSurface,
    requestedSurface: AutomationTurnSurface,
  ) {
    super(
      `Cannot switch automation surface within one TUI turn (locked=${lockedSurface}, requested=${requestedSurface}). Send the request as a new turn.`,
    )
    this.name = 'AutomationTurnSurfaceTransitionError'
    this.lockedSurface = lockedSurface
    this.requestedSurface = requestedSurface
  }
}

function requestedAutomationSurface(
  prompt: string,
): AutomationTurnSurface | undefined {
  const trimmed = prompt.trim()
  const explicitSkillName = trimmed.match(/^\/([^\s]+)/u)?.[1]
  if (explicitSkillName) {
    const explicitSurface = automationSurfaceForSkillName(explicitSkillName)
    if (explicitSurface !== undefined) return explicitSurface
  }
  return classifyAutomationSurfaceIntent(prompt)?.backend
}

/**
 * One QueryEngine is created for one interactive turn, but steer
 * can reach it by two physical paths: a tool-round provider or a fallback
 * submitMessage call. This lock is shared by both paths so arrival timing can
 * never select a different automation capability.
 *
 * An unclassified follow-up inherits the locked surface. It is not interpreted
 * as a request to return to the ordinary surface: short steers such as
 * "continue" or "click the next button" intentionally omit the backend name.
 */
export class AutomationTurnSurfaceLock {
  private locked:
    | {
        surface: LockedAutomationTurnSurface
        commandAllowRules: readonly string[] | undefined
      }
    | undefined

  get lockedSurface(): LockedAutomationTurnSurface | undefined {
    return this.locked?.surface
  }

  /**
   * Reject an explicit cross-surface steer before it can be injected or shown
   * as accepted. The first submit has not locked yet, so this is a no-op then;
   * `acceptProcessedSurface` records the authoritative processed result.
   */
  assertSteerPrompt(prompt: string): void {
    if (!this.locked) return
    const requested = requestedAutomationSurface(prompt)
    if (requested === undefined || requested === this.locked.surface) return
    throw new AutomationTurnSurfaceTransitionError(
      this.locked.surface,
      requested,
    )
  }

  /**
   * Lock the first processed surface, then resolve every later submit onto it.
   * Automation allow rules are frozen with the first surface so a later steer
   * cannot widen the tool family even when it repeats a skill invocation.
   */
  acceptProcessedSurface(
    processedSurface: AutomationTurnSurface | undefined,
    commandAllowRules: readonly string[] | undefined,
  ): AutomationTurnSurfaceResolution {
    if (!this.locked) {
      this.locked = {
        surface: processedSurface ?? 'ordinary',
        commandAllowRules:
          processedSurface === undefined
            ? undefined
            : commandAllowRules === undefined
              ? undefined
              : [...commandAllowRules],
      }
    } else if (
      processedSurface !== undefined &&
      processedSurface !== this.locked.surface
    ) {
      throw new AutomationTurnSurfaceTransitionError(
        this.locked.surface,
        processedSurface,
      )
    }

    if (this.locked.surface === 'ordinary') {
      return {
        automationSurface: undefined,
        commandAllowRules:
          commandAllowRules === undefined
            ? undefined
            : [...commandAllowRules],
      }
    }
    return {
      automationSurface: this.locked.surface,
      commandAllowRules:
        this.locked.commandAllowRules === undefined
          ? undefined
          : [...this.locked.commandAllowRules],
    }
  }
}

export function createAutomationTurnSurfaceLock(): AutomationTurnSurfaceLock {
  return new AutomationTurnSurfaceLock()
}
