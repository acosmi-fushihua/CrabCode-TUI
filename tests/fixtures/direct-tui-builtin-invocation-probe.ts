import { getDirectTuiBuiltInCommandDefinitions } from '../../src/cli/headlessCommands.js'

const invocations = [
  ...new Set(
    getDirectTuiBuiltInCommandDefinitions().flatMap(command => [
      command.name,
      ...(command.aliases ?? []),
    ]),
  ),
].sort()
process.stdout.write(`${JSON.stringify(invocations)}\n`)
