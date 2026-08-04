import { writeFileSync } from 'node:fs'

const mode = process.argv[2]
const pidFile = process.argv[3]
if (
  !new Set(['closed', 'exit', 'hang', 'partial', 'stderr', 'late']).has(mode) ||
  !pidFile
) {
  throw new Error(
    'usage: inherited-stdio-process.ts <closed|exit|hang|partial|stderr|late> <pid-file>',
  )
}

if (mode === 'closed') {
  process.stdout.write('{"closed":true}\n')
  process.exit(0)
}

const descendant = Bun.spawn({
  cmd: [
    process.execPath,
    '-e',
    mode === 'late'
      ? "await Bun.sleep(250); process.stdout.write('late-output\\n')"
      : 'await Bun.sleep(60_000)',
  ],
  stdin: 'ignore',
  stdout: 'inherit',
  stderr: 'inherit',
})
writeFileSync(pidFile, String(descendant.pid))
process.stdout.write(
  `${JSON.stringify({ descendantPid: descendant.pid })}${mode === 'partial' ? '' : '\n'}`,
)
if (mode === 'stderr') process.stderr.write('fixture stderr\n')

if (mode !== 'hang') process.exit(0)
await Bun.sleep(60_000)
