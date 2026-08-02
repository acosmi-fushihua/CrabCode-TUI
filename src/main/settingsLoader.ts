import chalk from 'chalk';
import { readFileSync } from 'fs';
import { profileCheckpoint } from '../utils/startupProfiler.js';
import { eagerParseCliFlag } from 'src/utils/cliArgs.js';
import { errorMessage } from 'src/utils/errors.js';
import { getFsImplementation, safeResolvePath } from 'src/utils/fsOperations.js';
import { safeParseJSON } from '../utils/json.js';
import { logError } from '../utils/log.js';
import { resetSettingsCache } from '../utils/settings/settingsCache.js';
import { writeFileSync_DEPRECATED } from '../utils/slowOperations.js';
import { generateTempFilePath } from '../utils/tempfile.js';
import { isENOENT } from 'src/utils/errors.js';
import { setAllowedSettingSources, setFlagSettingsPath } from '../bootstrap/state.js';
import { parseSettingSourcesFlag } from 'src/utils/settings/constants.js';

export function loadSettingsFromFlag(settingsFile: string): void {
  try {
    const trimmedSettings = settingsFile.trim();
    const looksLikeJson = trimmedSettings.startsWith('{') && trimmedSettings.endsWith('}');
    let settingsPath: string;
    if (looksLikeJson) {
      // It's a JSON string - validate and create temp file
      const parsedJson = safeParseJSON(trimmedSettings);
      if (!parsedJson) {
        process.stderr.write(chalk.red('Error: Invalid JSON provided to --settings\n'));
        process.exit(1);
      }

      // Create a temporary file and write the JSON to it.
      // Use a content-hash-based path instead of random UUID to avoid
      // busting the Acosmi API prompt cache. The settings path ends up
      // in the Bash tool's sandbox denyWithinAllow list, which is part of
      // the tool description sent to the API. A random UUID per subprocess
      // changes the tool description on every query() call, invalidating
      // the cache prefix and causing a 12x input token cost penalty.
      // The content hash ensures identical settings produce the same path
      // across process boundaries (each SDK query() spawns a new process).
      settingsPath = generateTempFilePath('crabcode-settings', '.json', {
        contentHash: trimmedSettings
      });
      writeFileSync_DEPRECATED(settingsPath, trimmedSettings, 'utf8');
    } else {
      // It's a file path - resolve and validate by attempting to read
      const {
        resolvedPath: resolvedSettingsPath
      } = safeResolvePath(getFsImplementation(), settingsFile);
      try {
        readFileSync(resolvedSettingsPath, 'utf8');
      } catch (e) {
        if (isENOENT(e)) {
          process.stderr.write(chalk.red(`Error: Settings file not found: ${resolvedSettingsPath}\n`));
          process.exit(1);
        }
        throw e;
      }
      settingsPath = resolvedSettingsPath;
    }
    setFlagSettingsPath(settingsPath);
    resetSettingsCache();
  } catch (error) {
    if (error instanceof Error) {
      logError(error);
    }
    process.stderr.write(chalk.red(`Error processing settings: ${errorMessage(error)}\n`));
    process.exit(1);
  }
}
export function loadSettingSourcesFromFlag(settingSourcesArg: string): void {
  try {
    const sources = parseSettingSourcesFlag(settingSourcesArg);
    setAllowedSettingSources(sources);
    resetSettingsCache();
  } catch (error) {
    if (error instanceof Error) {
      logError(error);
    }
    process.stderr.write(chalk.red(`Error processing --setting-sources: ${errorMessage(error)}\n`));
    process.exit(1);
  }
}

/**
 * Parse and load settings flags early, before init()
 * This ensures settings are filtered from the start of initialization
 */
export function eagerLoadSettings(): void {
  profileCheckpoint('eagerLoadSettings_start');
  // Parse --settings flag early to ensure settings are loaded before init()
  const settingsFile = eagerParseCliFlag('--settings');
  if (settingsFile) {
    loadSettingsFromFlag(settingsFile);
  }

  // Parse --setting-sources flag early to control which sources are loaded
  const settingSourcesArg = eagerParseCliFlag('--setting-sources');
  if (settingSourcesArg !== undefined) {
    loadSettingSourcesFromFlag(settingSourcesArg);
  }
  profileCheckpoint('eagerLoadSettings_end');
}
