// This file represents useful wrappers over node:child_process
// These wrappers ease error handling and cross-platform compatbility
// By using execa, Windows automatically gets shell escaping + BAT / CMD handling

import { type ExecaError, execa } from 'execa'
import { isAbsolute } from 'node:path'
import { getCwd } from '../utils/cwd.js'
import { logError } from './log.js'

export { execSyncWithDefaults_DEPRECATED } from './execFileNoThrowPortable.js'

const MS_IN_SECOND = 1000
const SECONDS_IN_MINUTE = 60

type ExecFileOptions = {
  abortSignal?: AbortSignal
  timeout?: number
  preserveOutputOnError?: boolean
  // Setting useCwd=false avoids circular dependencies during initialization
  // getCwd() -> PersistentShell -> logEvent() -> execFileNoThrow
  useCwd?: boolean
  env?: NodeJS.ProcessEnv
  /** When false, use exactly `env` instead of merging it with process.env. */
  extendEnv?: boolean
  stdin?: 'ignore' | 'inherit' | 'pipe'
  input?: string
  /**
   * Opt-in descendant cleanup for trusted bounded subprocesses. On Unix the
   * child starts in its own process group; on Windows a native wrapper creates
   * the target suspended inside a kill-on-close Job Object. Abort/timeout does
   * not resolve until the direct wrapper is reaped and its Job is closed.
   */
  killProcessTree?: boolean
  /** Invoked after the direct child has emitted its successful spawn event. */
  onSpawn?: () => void
}

export function execFileNoThrow(
  file: string,
  args: string[],
  options: ExecFileOptions = {
    timeout: 10 * SECONDS_IN_MINUTE * MS_IN_SECOND,
    preserveOutputOnError: true,
    useCwd: true,
  },
): Promise<{ stdout: string; stderr: string; code: number; error?: string }> {
  return execFileNoThrowWithCwd(file, args, {
    abortSignal: options.abortSignal,
    timeout: options.timeout,
    preserveOutputOnError: options.preserveOutputOnError,
    cwd: options.useCwd ? getCwd() : undefined,
    env: options.env,
    extendEnv: options.extendEnv,
    stdin: options.stdin,
    input: options.input,
    killProcessTree: options.killProcessTree,
    onSpawn: options.onSpawn,
  })
}

type ExecFileWithCwdOptions = {
  abortSignal?: AbortSignal
  timeout?: number
  preserveOutputOnError?: boolean
  maxBuffer?: number
  cwd?: string
  env?: NodeJS.ProcessEnv
  /** When false, use exactly `env` instead of merging it with process.env. */
  extendEnv?: boolean
  shell?: boolean | string | undefined
  stdin?: 'ignore' | 'inherit' | 'pipe'
  input?: string
  /** See ExecFileOptions.killProcessTree. Disabled by default. */
  killProcessTree?: boolean
  /** See ExecFileOptions.onSpawn. */
  onSpawn?: () => void
}

type ExecaResultWithError = {
  shortMessage?: string
  signal?: string
}

/**
 * Extracts a human-readable error message from an execa result.
 *
 * Priority order:
 * 1. shortMessage - execa's human-readable error (e.g., "Command failed with exit code 1: ...")
 *    This is preferred because it already includes signal info when a process is killed,
 *    making it more informative than just the signal name.
 * 2. signal - the signal that killed the process (e.g., "SIGTERM")
 * 3. errorCode - fallback to just the numeric exit code
 */
function getErrorMessage(
  result: ExecaResultWithError,
  errorCode: number,
): string {
  if (result.shortMessage) {
    return result.shortMessage
  }
  if (typeof result.signal === 'string') {
    return result.signal
  }
  return String(errorCode)
}

/**
 * execFile, but always resolves (never throws)
 */
export function execFileNoThrowWithCwd(
  file: string,
  args: string[],
  {
    abortSignal,
    timeout: finalTimeout = 10 * SECONDS_IN_MINUTE * MS_IN_SECOND,
    preserveOutputOnError: finalPreserveOutput = true,
    cwd: finalCwd,
    env: finalEnv,
    extendEnv: finalExtendEnv,
    maxBuffer,
    shell,
    stdin: finalStdin,
    input: finalInput,
    killProcessTree: finalKillProcessTree = false,
    onSpawn: finalOnSpawn,
  }: ExecFileWithCwdOptions = {
    timeout: 10 * SECONDS_IN_MINUTE * MS_IN_SECOND,
    preserveOutputOnError: true,
    maxBuffer: 1_000_000,
  },
): Promise<{ stdout: string; stderr: string; code: number; error?: string }> {
  return new Promise((resolve) => {
    let spawnFile = file;
    let spawnArgs = args;
    if (finalKillProcessTree && process.platform === "win32") {
      const inheritedEnv =
        finalExtendEnv === false ? finalEnv : { ...process.env, ...finalEnv };
      const helper = inheritedEnv?.CRABCODE_PROCESS_TREE_EXECUTABLE;
      if (typeof helper !== "string" || !isAbsolute(helper)) {
        resolve({
          stdout: "",
          stderr: "",
          code: 1,
          error:
            "race-free Windows process-tree helper is unavailable; refusing to spawn an unowned child tree",
        });
        return;
      }
      spawnFile = helper;
      spawnArgs = ["process-tree-exec", "--", file, ...args];
    }
    // Use execa for cross-platform .bat/.cmd compatibility on Windows. Tree
    // mode owns cancellation/timeout so execa cannot kill only the leader and
    // orphan hook/gpg/ssh/credential-helper descendants.
    const child = execa(spawnFile, spawnArgs, {
      maxBuffer,
      cancelSignal: finalKillProcessTree ? undefined : abortSignal,
      timeout: finalKillProcessTree ? undefined : finalTimeout,
      cwd: finalCwd,
      env: finalEnv,
      extendEnv: finalExtendEnv,
      shell,
      stdin: finalStdin,
      input: finalInput,
      detached: finalKillProcessTree && process.platform !== "win32",
      reject: false, // Don't throw on non-zero exit codes
    });
    const onSpawn = (): void => {
      finalOnSpawn?.();
    };
    child.once("spawn", onSpawn);

    let treeKillPromise: Promise<void> | null = null;
    const killTree = (): Promise<void> => {
      if (treeKillPromise) return treeKillPromise;
      const pid = child.pid;
      treeKillPromise = new Promise<void>((treeKillResolve) => {
        if (!pid) {
          treeKillResolve();
          return;
        }
        if (process.platform !== "win32") {
          try {
            // `detached: true` makes the child the leader of a fresh process
            // group. Negative PID is the only race-resistant way to kill
            // descendants even when the direct child is already exiting.
            process.kill(-pid, "SIGKILL");
          } catch (error) {
            if (
              typeof error !== "object" ||
              error === null ||
              !("code" in error) ||
              error.code !== "ESRCH"
            ) {
              logError(error);
            }
          }
          treeKillResolve();
          return;
        }
        // The direct child is the native app-server wrapper. It created the
        // real target suspended and owns a KILL_ON_JOB_CLOSE Job Object, so
        // terminating the wrapper closes that handle and deterministically
        // tears down descendants even if the target leader already exited.
        try {
          child.kill("SIGKILL");
        } catch {
          // Already gone; its Job handle has already been closed by the OS.
        }
        treeKillResolve();
      });
      return treeKillPromise;
    };

    const onAbort = (): void => {
      void killTree();
    };
    const onLeaderExit = (): void => {
      // A Git hook/helper can daemonize and inherit stdout/stderr. Execa waits
      // for those pipes even after Git exits, so close the whole operation
      // group as soon as the leader reaches its terminal state.
      void killTree();
    };
    if (finalKillProcessTree) child.once("exit", onLeaderExit);
    if (finalKillProcessTree && abortSignal) {
      if (abortSignal.aborted) onAbort();
      else abortSignal.addEventListener("abort", onAbort, { once: true });
    }
    const timeoutHandle =
      finalKillProcessTree &&
      typeof finalTimeout === "number" &&
      Number.isFinite(finalTimeout) &&
      finalTimeout > 0
        ? setTimeout(() => {
            void killTree();
          }, finalTimeout)
        : null;
    timeoutHandle?.unref();

    const cleanup = (): void => {
      if (timeoutHandle) clearTimeout(timeoutHandle);
      child.removeListener("spawn", onSpawn);
      child.removeListener("exit", onLeaderExit);
      abortSignal?.removeEventListener("abort", onAbort);
    };

    child
      .then((result) => {
        cleanup();
        const finish = async (): Promise<void> => {
          if (treeKillPromise) await treeKillPromise;
          if (result.failed) {
            if (finalPreserveOutput) {
              const errorCode = result.exitCode ?? 1;
              void resolve({
                stdout: result.stdout || "",
                stderr: result.stderr || "",
                code: errorCode,
                error: getErrorMessage(
                  result as unknown as ExecaResultWithError,
                  errorCode,
                ),
              });
            } else {
              void resolve({
                stdout: "",
                stderr: "",
                code: result.exitCode ?? 1,
              });
            }
          } else {
            void resolve({
              stdout: result.stdout,
              stderr: result.stderr,
              code: 0,
            });
          }
        };
        void finish();
      })
      .catch((error: ExecaError) => {
        cleanup();
        logError(error);
        const finish = async (): Promise<void> => {
          if (treeKillPromise) await treeKillPromise;
          void resolve({ stdout: "", stderr: "", code: 1 });
        };
        void finish();
      });
  });
}
