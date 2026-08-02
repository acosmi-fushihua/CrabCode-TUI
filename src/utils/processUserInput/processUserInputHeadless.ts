import {
  processUserInputCore,
  type ProcessUserInputRuntime,
} from './processUserInputCore.js'

export type {
  ProcessUserInputBaseResult,
  ProcessUserInputContext,
} from './processUserInputCore.js'

const headlessRuntime: ProcessUserInputRuntime = {
  loadSlashCommandProcessor: () =>
    import('./processSlashCommandHeadless.js'),
  loadBashCommandProcessor: () =>
    import('./processBashCommandHeadless.js'),
}

export async function processUserInput(
  args: Parameters<typeof processUserInputCore>[0],
): ReturnType<typeof processUserInputCore> {
  return processUserInputCore(args, headlessRuntime)
}
