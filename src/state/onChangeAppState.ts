import { setMainLoopModelOverride } from '../bootstrap/state.js'
import { clearApiKeyHelperCache } from '../utils/auth.js'
import { getGlobalConfig, saveGlobalConfig } from '../utils/config.js'
import { toError } from '../utils/errors.js'
import { logError } from '../utils/log.js'
import { applyConfigEnvironmentVariables } from '../utils/managedEnv.js'
import { persistModelSelectionTransaction } from '../utils/model/customModelSelectionTransaction.js'
import { notifyPermissionModeChanged } from '../utils/sessionState.js'
import {
  getSettingsForSource,
  updateSettingsForSource,
} from '../utils/settings/settings.js'
import { resetSettingsCache } from '../utils/settings/settingsCache.js'
import type { AppState } from './AppStateStore.js'

export function onChangeAppState({
  newState,
  oldState,
}: {
  newState: AppState
  oldState: AppState
}) {
  const modelChanged = newState.mainLoopModel !== oldState.mainLoopModel
  if (modelChanged) {
    const persisted = persistModelSelectionTransaction(
      oldState.mainLoopModel,
      newState.mainLoopModel,
      {
        readUserSettings: () => getSettingsForSource('userSettings'),
        resetUserSettingsCache: resetSettingsCache,
        writeUserModel: model =>
          updateSettingsForSource('userSettings', {
            model: model ?? undefined,
          }),
      },
    )
    if (!persisted.ok) {
      logError(
        new Error(
          `main loop model selection was not persisted: ${persisted.reason}`,
        ),
      )
      return false
    }
    setMainLoopModelOverride(newState.mainLoopModel)
  }

  // Centralize permission-mode notifications so every TUI mutation path emits
  // the same SDK status update.
  const prevMode = oldState.toolPermissionContext.mode
  const newMode = newState.toolPermissionContext.mode
  if (prevMode !== newMode) {
    notifyPermissionModeChanged(newMode)
  }

  // expandedView → persist as showExpandedTodos + showSpinnerTree for backwards compat.
  // W-GOAL-CONSOLE PR-B: 'goalConsole' is an ephemeral overlay (like a transient
  // modal) and must NOT persist across sessions, nor clobber the persisted
  // tasks/teammates preference while it is open. Skip the config write for any
  // transition that involves 'goalConsole' — the underlying persisted flags
  // stay as the user last left them and the panel restores to 'none' on close.
  if (
    newState.expandedView !== oldState.expandedView &&
    newState.expandedView !== 'goalConsole' &&
    oldState.expandedView !== 'goalConsole'
  ) {
    const showExpandedTodos = newState.expandedView === 'tasks'
    const showSpinnerTree = newState.expandedView === 'teammates'
    if (
      getGlobalConfig().showExpandedTodos !== showExpandedTodos ||
      getGlobalConfig().showSpinnerTree !== showSpinnerTree
    ) {
      saveGlobalConfig(current => ({
        ...current,
        showExpandedTodos,
        showSpinnerTree,
      }))
    }
  }

  // verbose
  if (
    newState.verbose !== oldState.verbose &&
    getGlobalConfig().verbose !== newState.verbose
  ) {
    const verbose = newState.verbose
    saveGlobalConfig(current => ({
      ...current,
      verbose,
    }))
  }

  // tungstenPanelVisible (ant-only tmux panel sticky toggle)
  if (process.env.USER_TYPE === 'ant') {
    if (
      newState.tungstenPanelVisible !== oldState.tungstenPanelVisible &&
      newState.tungstenPanelVisible !== undefined &&
      getGlobalConfig().tungstenPanelVisible !== newState.tungstenPanelVisible
    ) {
      const tungstenPanelVisible = newState.tungstenPanelVisible
      saveGlobalConfig(current => ({ ...current, tungstenPanelVisible }))
    }
  }

  // settings: clear auth-related caches when settings change
  // This ensures apiKeyHelper credential changes take effect immediately
  if (newState.settings !== oldState.settings) {
    try {
      clearApiKeyHelperCache()

      // Re-apply on every settings change, not only settings.env. Structured
      // customModel → customModels migration must retract bridge-owned
      // CRABCODE_CUSTOM_* values immediately or this long-lived process keeps
      // provider=custom and hides gateway models until restart.
      applyConfigEnvironmentVariables()
    } catch (error) {
      logError(toError(error))
    }
  }
}
