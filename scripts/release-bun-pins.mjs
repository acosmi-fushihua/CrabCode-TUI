const bunLicensePins = Object.freeze({
  '1.3.11': Object.freeze({
    url: 'https://raw.githubusercontent.com/oven-sh/bun/bun-v1.3.11/LICENSE.md',
    sha256: '7068a9711ef8196d654e143447ed7976b3678ce21145b9da16e1f786528f15bb',
  }),
  '1.3.14': Object.freeze({
    url: 'https://raw.githubusercontent.com/oven-sh/bun/bun-v1.3.14/LICENSE.md',
    sha256: '2c6160ec8fb853f7e8f97d9b249e756c9b0ac44860a68b6bf4f1b0bcbc5c3741',
  }),
})

export const defaultBunRelease = Object.freeze({
  version: '1.3.11',
  license: bunLicensePins['1.3.11'],
})

export const x64DarwinBunRelease = Object.freeze({
  version: '1.3.14',
  asset: 'bun-darwin-x64-baseline.zip',
  root: 'bun-darwin-x64-baseline',
  url: 'https://github.com/oven-sh/bun/releases/download/bun-v1.3.14/bun-darwin-x64-baseline.zip',
  sha256: '3e35ad6f53971a9834bf9e6786e2adf72b5f1921cc9a9c5fde073d2972944076',
  bytes: 26_509_145,
  executableBytes: 69_173_328,
  executableSha256: 'ea2f223e94bb2f4bf3050895113c3cf346438f6fa0501c8532284e063f72f7a0',
  license: bunLicensePins['1.3.14'],
})

export function bunReleaseForPlatform(platform) {
  return platform === 'x64-darwin' ? x64DarwinBunRelease : defaultBunRelease
}

export function bunNoAvxBaselineWarning(release = x64DarwinBunRelease) {
  return (
    'warn: CPU lacks AVX support, strange crashes may occur. Reinstall Bun or use *-baseline build:\n' +
    `  ${release.url}`
  )
}
