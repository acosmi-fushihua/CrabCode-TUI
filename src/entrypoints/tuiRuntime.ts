import { installStreamJsonStdoutGuard } from '../utils/streamJsonStdoutGuard.js'
import type { TuiRuntimeOptions } from '../cli/tuiRuntimeOptions.js'
import type { NativeTuiRendererSession } from './nativeTuiRendererSession.js'

type TuiRuntimeEntrypointDependencies = {
  parseOptions?: (argv: readonly string[]) => Promise<TuiRuntimeOptions>
  runRuntime?: (
    options: TuiRuntimeOptions,
    rendererSession: NativeTuiRendererSession,
  ) => Promise<void>
  startRendererSession?: () => Promise<NativeTuiRendererSession>
}

/**
 * Start the process-owned native-TUI runtime.
 *
 * Every TUI startup installs the guard before importing the option parser or
 * the backend bootstrap: an eager dependency banner or accidental console.log
 * can therefore never precede/corrupt the first StructuredIO control response.
 */
export async function runTuiRuntimeEntrypoint(
  argv: readonly string[] = process.argv,
  dependencies: TuiRuntimeEntrypointDependencies = {},
): Promise<void> {
  // This executable is a trusted product boundary. Never inherit a caller's
  // unrelated surface identity (for example local-agent or desktop).
  process.env.CRABCODE_ENTRYPOINT = 'cli'

  installStreamJsonStdoutGuard()
  const startRendererSession =
    dependencies.startRendererSession ??
    (async () => {
      const { startNativeTuiRendererSession } = await import(
        './nativeTuiRendererSession.js'
      )
      return startNativeTuiRendererSession()
    })
  // Establish one lossless stdin owner before importing the option parser.
  // Renderer context is deliberately deferred until backend configuration
  // initialization; historical workspace trust remains at its single
  // post-setup point after the final cwd has been selected.
  const rendererSession = await startRendererSession()
  const parseOptions =
    dependencies.parseOptions ??
    (async (runtimeArgv: readonly string[]) => {
      const { parseTuiRuntimeOptions } = await import(
        '../cli/tuiRuntimeOptions.js'
      )
      return parseTuiRuntimeOptions(runtimeArgv)
    })
  const options = await parseOptions(argv)
  if (options.bare) process.env.CRABCODE_SIMPLE = '1'

  const runRuntime =
    dependencies.runRuntime ??
    (async (
      runtimeOptions: TuiRuntimeOptions,
      runtimeRendererSession: NativeTuiRendererSession,
    ) => {
      const { runTuiRuntime } = await import('../cli/tuiRuntimeBootstrap.js')
      await runTuiRuntime(runtimeOptions, runtimeRendererSession)
    })
  await runRuntime(options, rendererSession)
}

if (import.meta.main) {
  // Keep the entry module itself synchronously evaluated. The v1.0.23/v1.0.24
  // package evidence showed Bun 1.3.11 stalling before the first frame, and
  // upstream later fixed related TLA/dynamic-import deadlock classes. Do not
  // retain the application side of that risk. The runtime promise still owns
  // the full lifecycle; a rejected bootstrap remains process-fatal after
  // stderr is flushed.
  void runTuiRuntimeEntrypoint().catch(error => {
    const message =
      error instanceof Error ? (error.stack ?? error.message) : String(error)
    process.exitCode = 1
    process.stderr.write(`${message}\n`, () => process.exit(1))
  })
}
