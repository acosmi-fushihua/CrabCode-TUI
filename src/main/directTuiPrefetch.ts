import { getIsNonInteractiveSession } from '../bootstrap/state.js'
import { getSystemContext, getUserContext } from '../context.js'
import { prefetchOfficialMcpUrls } from '../services/mcp/officialRegistry.js'
import { getCwd } from '../utils/cwd.js'
import { isBareMode, isEnvTruthy } from '../utils/envUtils.js'
import {
  getCachedEnabledModels,
  refreshModelCapabilities,
} from '../utils/model/modelCapabilities.js'
import { countFilesRoundedRg } from '../utils/ripgrep.js'
import { settingsChangeDetector } from '../utils/settings/changeDetector.js'
import { skillChangeDetector } from '../utils/skills/skillChangeDetector.js'
import { logForDiagnosticsNoPII } from '../utils/diagLogs.js'
import { checkHasTrustDialogAccepted } from '../utils/config.js'
import { initUser } from '../utils/user.js'

function prefetchSystemContextIfSafe(): void {
  if (getIsNonInteractiveSession()) {
    logForDiagnosticsNoPII(
      'info',
      'prefetch_system_context_non_interactive',
    )
    void getSystemContext()
    return
  }
  if (checkHasTrustDialogAccepted()) {
    logForDiagnosticsNoPII('info', 'prefetch_system_context_has_trust')
    void getSystemContext()
  } else {
    logForDiagnosticsNoPII('info', 'prefetch_system_context_skipped_no_trust')
  }
}

/**
 * Backend-only counterpart of main/prefetch.ts for the native TUI.
 *
 * Tips and startup callouts are presentation inventory owned by Rust. Context,
 * model/MCP caches and settings/skill change detectors remain the unchanged
 * TypeScript backend's responsibility.
 */
export function startDirectTuiDeferredPrefetches(): void {
  if (
    isEnvTruthy(process.env.CRABCODE_EXIT_AFTER_FIRST_RENDER) ||
    isBareMode()
  ) {
    return
  }

  void initUser()
  void getUserContext()
  prefetchSystemContextIfSafe()
  void countFilesRoundedRg(getCwd(), AbortSignal.timeout(3000), [])
  void prefetchOfficialMcpUrls()
  if (getCachedEnabledModels().length === 0) {
    void refreshModelCapabilities()
  }
  void settingsChangeDetector.initialize()
  void skillChangeDetector.initialize()

}
