// src/utils/bash/bashParser.ts — Barrel re-export file
// All implementation has been split into src/utils/bash/bashParser/ submodules.

// Types and constants
export { type TsNode, SHELL_KEYWORDS } from './bashParser/types.js'

// Module API
export {
  ensureParserInitialized,
  getParserModule,
} from './bashParser/parserCore.js'
