import chalk from 'chalk'
import { COMMON_INFO_ARGS } from '../../constants/xml.js'
import { logEvent } from '../../services/analytics/index.js'
import type {
  LocalCommandCall,
  LocalCommandResult,
} from '../../types/command.js'
import { getSmallFastModel } from '../../utils/model/model.js'
import { updateSettingsForSource } from '../../utils/settings/settings.js'

/**
 * /smallmodel — configure the small/fast model for internal tasks.
 *
 * This is the renderer-neutral equivalent of the fixed React effect. It keeps
 * the same settings authority, accepted arguments, telemetry, and visible
 * result without mounting JSX or introducing a new control request.
 */
export function executeSmallModelCommand(
  args: string,
): Extract<LocalCommandResult, { type: 'text' }> {
  const model = args?.trim() || ''

  if (!model || COMMON_INFO_ARGS.includes(model)) {
    const current = getSmallFastModel()
    return {
      type: 'text',
      value: current
        ? `Small model: ${chalk.bold(current)}`
        : 'Small model: not set (using SDK default)',
    }
  }

  if (model === 'default' || model === 'reset' || model === 'clear') {
    const { error } = updateSettingsForSource('userSettings', {
      smallModel: undefined,
    })
    if (error) {
      throw new Error(`Failed to reset small model: ${error.message}`, {
        cause: error,
      })
    }
    return {
      type: 'text',
      value: `Small model reset to SDK default (${chalk.bold(getSmallFastModel() || 'auto')})`,
    }
  }

  const { error } = updateSettingsForSource('userSettings', {
    smallModel: model,
  })
  if (error) {
    throw new Error(`Failed to set small model: ${error.message}`, {
      cause: error,
    })
  }
  logEvent('tengu_smallmodel_set', {})
  return {
    type: 'text',
    value: `Small model set to ${chalk.bold(model)}`,
  }
}

export const call: LocalCommandCall = async args =>
  executeSmallModelCommand(args)
