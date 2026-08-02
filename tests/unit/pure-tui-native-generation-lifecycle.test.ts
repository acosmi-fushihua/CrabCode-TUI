import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

const launcher = readFileSync(
  new URL('../../crates/crabcode-cli/src/pure_tui_launcher.rs', import.meta.url),
  'utf8',
)
const generation = readFileSync(
  new URL('../../crates/crabcode-cli/src/native_generation.rs', import.meta.url),
  'utf8',
)
const cargo = readFileSync(
  new URL('../../crates/crabcode-cli/Cargo.toml', import.meta.url),
  'utf8',
)

describe('pure TUI native generation lifecycle source contract', () => {
  test('the existing marker protocol is restored without a second update protocol', () => {
    expect(generation).toContain(
      'pub(crate) const CURRENT_GENERATION_MARKER: &str = ".current"',
    )
    expect(generation).toContain(
      'pub(crate) const STABLE_LAUNCHER_PROTOCOL_MARKER: &str = ".launcher-v1"',
    )
    expect(generation).toContain(
      'pub(crate) const STABLE_LAUNCHER_PENDING_MARKER: &str = ".launcher-v1.pending"',
    )
    expect(generation).not.toContain('crabcode-app-server')
    expect(generation).not.toContain('frontend/tauri')
  })

  test('Windows activation remains the exact native two-stage breakaway flow', () => {
    const broker = generation.slice(
      generation.indexOf(
        'pub(crate) fn launch_prepared_windows_activation_helper',
      ),
      generation.indexOf('fn move_replace_write_through'),
    )
    const commit = generation.slice(
      generation.indexOf(
        'pub(crate) fn activate_prepared_windows_launcher',
      ),
      generation.indexOf('#[cfg(test)]'),
    )

    expect(broker).toContain('CREATE_BREAKAWAY_FROM_JOB')
    expect(broker).toContain('CREATE_NEW_PROCESS_GROUP')
    expect(broker).toContain('DETACHED_PROCESS')
    expect(broker).toContain('observe_live_windows_activation_parent')
    expect(broker).toContain('windows_activation_commit_args')
    expect(broker).toContain('ERROR_ACCESS_DENIED')
    expect(broker).toContain(
      'current_windows_job_denies_explicit_breakaway()?',
    )
    expect(broker).not.toContain('cmd.exe')
    expect(broker).not.toContain('powershell')
    expect(broker).not.toContain('retrying without')

    expect(commit.match(/assert_windows_activation_request/g)).toHaveLength(2)
    expect(commit).toContain('wait_for_windows_activation_parent')
    expect(commit).toContain('parent_start_identity')
  })

  test('private activation is parsed before terminal policy and cannot accept extras', () => {
    const run = launcher.slice(
      launcher.indexOf('fn run()'),
      launcher.indexOf('enum PrivateNativeGenerationCommand'),
    )
    const parser = launcher.slice(
      launcher.indexOf('fn parse_private_native_generation_command'),
      launcher.indexOf('fn parse_private_parent_pid'),
    )

    expect(run.indexOf('handoff_to_current_generation()')).toBeGreaterThan(-1)
    expect(run.indexOf('handoff_to_current_generation()')).toBeLessThan(
      run.indexOf('std::io::stdin().is_terminal()'),
    )
    expect(
      run.indexOf('parse_private_native_generation_command(&raw_os_args)'),
    ).toBeLessThan(run.indexOf('std::io::stdin().is_terminal()'))
    expect(parser).toContain('args.len() != 5')
    expect(parser).toContain('args.len() != 7')
    expect(parser).toContain('args[1] != OsStr::new("--parent-pid")')
    expect(parser).toContain('args[3] != OsStr::new("--stable-launcher")')
    expect(parser).toContain(
      'args[3] != OsStr::new("--parent-start-identity")',
    )
    expect(parser).toContain('args[5] != OsStr::new("--stable-launcher")')
  })

  test('Windows stable handoff atomically owns the suspended direct generation', () => {
    const handoff = generation.slice(
      generation.indexOf('fn wait_for_windows_generation'),
      generation.indexOf('fn assert_pending_launcher_binding'),
    )
    const createJob = handoff.indexOf('StableLauncherJob::create()')
    const createSuspended = handoff.indexOf(
      'create_suspended_windows_generation(command, target, &job)',
    )
    const verifyMembership = handoff.indexOf(
      'job.assert_contains(&child.process)',
    )
    const enableBreakaway = handoff.indexOf(
      'StableLauncherJobPhase::GenerationRunning',
    )
    const resume = handoff.indexOf('child.resume_primary_thread()')

    expect(createJob).toBeGreaterThan(-1)
    expect(createJob).toBeLessThan(createSuspended)
    expect(createSuspended).toBeLessThan(verifyMembership)
    expect(verifyMembership).toBeLessThan(enableBreakaway)
    expect(enableBreakaway).toBeLessThan(resume)
    expect(handoff).toContain('PROC_THREAD_ATTRIBUTE_JOB_LIST')
    expect(handoff).toContain('CreateProcessW(')
    expect(handoff).toContain('ResumeThread(thread.0)')
    expect(handoff).toContain('JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE')
    expect(handoff).toContain('JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK')
    expect(handoff).toContain('child.terminate_and_reap(&job)')
    expect(handoff).not.toContain('AssignProcessToJobObject')
    expect(handoff).not.toContain('OpenThread')
  })

  test('the Rust validation subset matches the signed pure TUI bundle layout', () => {
    const validation = generation.slice(
      generation.indexOf('fn validate_generation_directory'),
      generation.indexOf('fn validate_generation_root'),
    )
    for (const path of [
      'dist/tui-runtime/index.js',
      'release-manifest.json',
      'release-manifest.sig',
    ]) {
      expect(validation).toContain(path)
    }
    expect(generation).toContain('"crabcode-tui.exe"')
    expect(generation).toContain('"acosmi-memory-orchestrator.exe"')
    expect(validation).not.toContain('dist/index.js')
  })

  test('the restored platform dependencies are restricted to lifecycle needs', () => {
    expect(cargo).toContain('dirs = { workspace = true }')
    expect(cargo).toContain("[target.'cfg(windows)'.dependencies]")
    for (const feature of [
      'Win32_Foundation',
      'Win32_Security',
      'Win32_Storage_FileSystem',
      'Win32_System_Console',
      'Win32_System_JobObjects',
      'Win32_System_Threading',
    ]) {
      expect(cargo).toContain(`"${feature}"`)
    }
    expect(cargo).not.toContain('acosmi-cmd-browser')
    expect(cargo).not.toContain('acosmi-cmd-cron')
    expect(cargo).not.toContain('acosmi-app-server')
  })
})
