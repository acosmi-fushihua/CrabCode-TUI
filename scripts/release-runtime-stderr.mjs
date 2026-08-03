import {
  bunNoAvxBaselineWarning as createBunNoAvxBaselineWarning,
  x64DarwinBunRelease,
} from './release-bun-pins.mjs'

export const bunNoAvxBaselineWarning = createBunNoAvxBaselineWarning()

function hasPinnedBaselineAuthority(materials) {
  const bun = materials?.runtime?.bun
  return (
    materials?.schemaVersion === 1 &&
    materials?.product === 'CrabCode TUI' &&
    materials?.platform === 'x64-darwin' &&
    bun?.version === x64DarwinBunRelease.version &&
    bun?.url === x64DarwinBunRelease.url &&
    bun?.sha256 === x64DarwinBunRelease.sha256
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
