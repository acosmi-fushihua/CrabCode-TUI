import { writeFileSync } from 'node:fs'

const mode = process.argv[2]
const pidFile = process.argv[3]
if (!new Set(['exit', 'hang', 'partial', 'stderr']).has(mode) || !pidFile) {
  throw new Error(
    'usage: inherited-stdio-process.ts <exit|hang|partial|stderr> <pid-file>',
  )
}

const descendant = Bun.spawn({
  cmd: [process.execPath, '-e', 'await Bun.sleep(60_000)'],
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
