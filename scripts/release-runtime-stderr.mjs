const baselineBun = Object.freeze({
  version: '1.3.11',
  url: 'https://github.com/oven-sh/bun/releases/download/bun-v1.3.11/bun-darwin-x64-baseline.zip',
  sha256: 'fb6739b08bf54550edaa7c824cd5b2dca45b6a06afef408443087a63105f6f8d',
})

export const bunNoAvxBaselineWarning =
  'warn: CPU lacks AVX support, strange crashes may occur. Reinstall Bun or use *-baseline build:\n' +
  '  https://github.com/oven-sh/bun/releases/download/bun-v1.3.11/bun-darwin-x64-baseline.zip'

function hasPinnedBaselineAuthority(materials) {
  const bun = materials?.runtime?.bun
  return (
    materials?.schemaVersion === 1 &&
    materials?.product === 'CrabCode TUI' &&
    materials?.platform === 'x64-darwin' &&
    bun?.version === baselineBun.version &&
    bun?.url === baselineBun.url &&
    bun?.sha256 === baselineBun.sha256
  )
}

export function classifyRuntimeStderr(stderr, releaseMaterials) {
  if (stderr === '') return 'empty'
  const exactWarning =
    stderr === bunNoAvxBaselineWarning ||
    stderr === `${bunNoAvxBaselineWarning}\n` ||
    stderr === `${bunNoAvxBaselineWarning}\r\n`
  if (exactWarning && hasPinnedBaselineAuthority(releaseMaterials)) {
    return 'classified-bun-baseline-no-avx-warning'
  }
  return 'unexpected'
}
