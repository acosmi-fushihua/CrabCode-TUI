import type {
  LocalCommandCall,
  LocalJSXCommandOnDone,
} from '../../types/command.js'
import { call as runTerminalSetup } from './terminalSetup.js'

/**
 * Route-private worker-slash adapter for the historical action.
 *
 * terminalSetup.call() is already renderer-neutral and always completes via
 * onDone; only its old command shape was local-jsx. Capture that established
 * completion result as a local-command text result so the direct runtime can
 * execute it without widening the local-jsx protocol.
 */
export const call: LocalCommandCall = async (args, context) => {
  let completion: string | undefined
  const onDone: LocalJSXCommandOnDone = result => {
    completion = result
  }

  await runTerminalSetup(onDone, context, args)
  if (completion === undefined) {
    throw new Error('terminal-setup completed without a result')
  }
  return { type: 'text', value: completion }
}
