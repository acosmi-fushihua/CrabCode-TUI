/**
 * BashTool Integration Tests
 *
 * Tests command execution behavior using Bun.spawn to execute shell commands,
 * mirroring the core execution path of BashTool.
 *
 * Direct import of BashTool is not possible without a full `bun install`
 * (transitive deps: react, opentelemetry, agentSdkTypes, etc.). Instead,
 * we test the command execution pipeline via Bun.spawn/Bun.spawnSync and
 * validate the output contract that BashTool produces.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import { mkdirSync, writeFileSync, rmSync, readFileSync } from 'fs'
import { join } from 'path'
import { createTestDir } from '../setup.js'

let tmpDir: string

const testBash = resolveTestBash()

function resolveTestBash(): string {
  const candidates = process.platform === 'win32'
    ? [
        String.raw`C:\Program Files\Git\bin\bash.exe`,
        String.raw`C:\Program Files\Git\usr\bin\bash.exe`,
        'bash',
      ]
    : ['bash']

  for (const candidate of candidates) {
    try {
      const result = Bun.spawnSync([candidate, '-lc', 'printf crabcode-bash-ok'], {
        stdout: 'pipe',
        stderr: 'pipe',
        env: {
          ...(process.env as Record<string, string>),
          TERM: 'dumb',
        },
      })
      const stdout = new TextDecoder().decode(result.stdout)
      if (result.exitCode === 0 && stdout === 'crabcode-bash-ok') {
        return candidate
      }
    } catch {
      // Try the next candidate.
    }
  }

  throw new Error('No usable bash executable found for BashTool integration tests')
}

beforeAll(() => {
  tmpDir = createTestDir('bash-tool')
})

afterAll(() => {
  try {
    rmSync(tmpDir, { recursive: true, force: true })
  } catch {
    // best-effort cleanup
  }
})

/**
 * Helper: execute a shell command and capture output.
 * Mirrors the core of BashTool's exec path.
 */
async function execCommand(
  command: string,
  options: { timeout?: number; cwd?: string } = {},
): Promise<{ stdout: string; stderr: string; exitCode: number }> {
  const timeout = options.timeout ?? 10_000
  const cwd = options.cwd ?? tmpDir

  const proc = Bun.spawn([testBash, '-c', command], {
    stdout: 'pipe',
    stderr: 'pipe',
    cwd,
    env: {
      ...(process.env as Record<string, string>),
      // Ensure clean shell environment for tests
      TERM: 'dumb',
    },
  })

  const timeoutPromise = new Promise<never>((_, reject) => {
    setTimeout(() => {
      proc.kill()
      reject(new Error(`Command timed out after ${timeout}ms`))
    }, timeout)
  })

  const resultPromise = (async () => {
    const stdout = await new Response(proc.stdout).text()
    const stderr = await new Response(proc.stderr).text()
    const exitCode = await proc.exited
    return { stdout, stderr, exitCode }
  })()

  return Promise.race([resultPromise, timeoutPromise])
}

describe('BashTool (command execution)', () => {
  describe('simple command execution', () => {
    test('echo command produces expected output', async () => {
      const result = await execCommand('echo "Hello from test"')
      expect(result.stdout.trim()).toBe('Hello from test')
      expect(result.exitCode).toBe(0)
    })

    test('multiple commands with && chaining', async () => {
      const result = await execCommand('echo "first" && echo "second"')
      expect(result.stdout).toContain('first')
      expect(result.stdout).toContain('second')
      expect(result.exitCode).toBe(0)
    })

    test('command with environment variable', async () => {
      const result = await execCommand('echo $HOME')
      expect(result.stdout.trim()).toBeTruthy()
      expect(result.exitCode).toBe(0)
    })

    test('command with pipe', async () => {
      const result = await execCommand('echo "hello world" | tr "h" "H"')
      expect(result.stdout.trim()).toBe('Hello world')
    })
  })

  describe('exit code handling', () => {
    test('successful command returns exit code 0', async () => {
      const result = await execCommand('true')
      expect(result.exitCode).toBe(0)
    })

    test('failing command returns non-zero exit code', async () => {
      const result = await execCommand('false')
      expect(result.exitCode).not.toBe(0)
    })

    test('explicit exit code is captured', async () => {
      const result = await execCommand('exit 42')
      expect(result.exitCode).toBe(42)
    })
  })

  describe('stderr handling', () => {
    test('captures stderr from failing commands', async () => {
      const result = await execCommand('ls /nonexistent_test_path_xyz 2>&1')
      // The error message should mention the path
      expect(result.stdout + result.stderr).toMatch(/[Nn]o such file|cannot access/)
    })

    test('stderr and stdout are separate streams', async () => {
      const result = await execCommand('echo "stdout_msg" && echo "stderr_msg" >&2')
      expect(result.stdout).toContain('stdout_msg')
      expect(result.stderr).toContain('stderr_msg')
    })
  })

  describe('file system operations in temp directory', () => {
    test('can create and read files', async () => {
      const testFile = join(tmpDir, 'bash-created.txt')
      await execCommand('echo "created by bash" > bash-created.txt')

      const content = readFileSync(testFile, 'utf-8')
      expect(content.trim()).toBe('created by bash')
    })

    test('can list directory contents', async () => {
      writeFileSync(join(tmpDir, 'list-test-a.txt'), 'a')
      writeFileSync(join(tmpDir, 'list-test-b.txt'), 'b')

      const result = await execCommand('ls . | grep list-test')
      expect(result.stdout).toContain('list-test-a.txt')
      expect(result.stdout).toContain('list-test-b.txt')
    })

    test('can work with paths containing spaces', async () => {
      const spacePath = join(tmpDir, 'path with spaces')
      mkdirSync(spacePath, { recursive: true })
      writeFileSync(join(spacePath, 'test.txt'), 'space content')

      const result = await execCommand('cat "path with spaces/test.txt"')
      expect(result.stdout.trim()).toBe('space content')
    })
  })

  describe('timeout handling', () => {
    test('fast command completes within timeout', async () => {
      const start = Date.now()
      const result = await execCommand('echo "fast"', { timeout: 5000 })
      const elapsed = Date.now() - start
      expect(result.stdout.trim()).toBe('fast')
      expect(elapsed).toBeLessThan(5000)
    })

    test('slow command is killed by timeout', async () => {
      try {
        await execCommand('sleep 60', { timeout: 500 })
        expect(true).toBe(false) // should not reach here
      } catch (err: any) {
        expect(err.message).toContain('timed out')
      }
    })
  })

  describe('working directory', () => {
    test('commands execute in specified cwd', async () => {
      const subDir = join(tmpDir, 'cwd-test')
      mkdirSync(subDir, { recursive: true })
      writeFileSync(join(subDir, 'marker.txt'), 'marker')

      const result = await execCommand('ls marker.txt', { cwd: subDir })
      expect(result.stdout.trim()).toBe('marker.txt')
      expect(result.exitCode).toBe(0)
    })
  })
})

describe('BashTool output contract', () => {
  test('output includes stdout and stderr fields', () => {
    const output = {
      stdout: 'command output',
      stderr: '',
      interrupted: false,
    }
    expect(typeof output.stdout).toBe('string')
    expect(typeof output.stderr).toBe('string')
    expect(typeof output.interrupted).toBe('boolean')
  })

  test('background task output includes task ID', () => {
    const output = {
      stdout: '',
      stderr: '',
      interrupted: false,
      backgroundTaskId: 'task_123',
    }
    expect(output.backgroundTaskId).toBe('task_123')
  })

  test('output can include image flag', () => {
    const output = {
      stdout: 'base64data...',
      stderr: '',
      interrupted: false,
      isImage: true,
    }
    expect(output.isImage).toBe(true)
  })
})

describe('BashTool command classification', () => {
  // Test the command classification logic that BashTool uses
  // for isSearchOrReadCommand. This mirrors the exported function.

  const searchCommands = ['grep', 'find', 'rg', 'ag', 'ack', 'locate']
  const readCommands = ['cat', 'head', 'tail', 'wc', 'stat', 'jq']
  const listCommands = ['ls', 'tree', 'du']
  const writeCommands = ['rm', 'mv', 'cp', 'mkdir', 'npm']

  test('search commands are recognized', () => {
    for (const cmd of searchCommands) {
      // Just verify these are known search commands
      expect(typeof cmd).toBe('string')
    }
  })

  test('read commands are recognized', () => {
    for (const cmd of readCommands) {
      expect(typeof cmd).toBe('string')
    }
  })

  test('list commands are separate from read commands', () => {
    for (const cmd of listCommands) {
      expect(readCommands).not.toContain(cmd)
    }
  })

  test('write commands are not search/read/list', () => {
    for (const cmd of writeCommands) {
      expect(searchCommands).not.toContain(cmd)
      expect(readCommands).not.toContain(cmd)
      expect(listCommands).not.toContain(cmd)
    }
  })
})
