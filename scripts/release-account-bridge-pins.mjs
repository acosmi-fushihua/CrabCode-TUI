const platformAssets = Object.freeze({
  'arm64-darwin': Object.freeze({
    asset: 'oauthapi-llm-arm64-darwin.zip',
    sha256: 'dba6f43a65cacaba461ad9537345f2dafb3224186978113f3b505d2bcf6fac61',
  }),
  'x64-darwin': Object.freeze({
    asset: 'oauthapi-llm-x64-darwin.zip',
    sha256: '37c6ce33981e8dadd5300cf4d050a1af93f2ab5737a3eeb885852c7166a74fb4',
  }),
  'arm64-linux': Object.freeze({
    asset: 'oauthapi-llm-arm64-linux.zip',
    sha256: '84da7d6364c1705380d5dfa311e4b3cf2e5f6f880cdc06b4f9daef6c2cc471a3',
  }),
  'x64-linux': Object.freeze({
    asset: 'oauthapi-llm-x64-linux.zip',
    sha256: 'a27fbc3817f231adcb46c3f7c282720dbf2024ccd61b0b61d9e3c887bf195aa5',
  }),
  'x64-win32': Object.freeze({
    asset: 'oauthapi-llm-x64-win32.zip',
    sha256: '98535cb660726399f5401801e145131adf18f1f269dc4125f1ba8d83dd752f23',
  }),
})

export const accountBridgeReleasePins = Object.freeze({
  repository: 'https://github.com/acosmi/crabcode',
  tag: 'v1.0.29',
  componentVersion: '7.2.71-crabcode.9',
  protocolVersion: 1,
  upstreamLockSha256: '248b0c095d5df8eb272a866b473fcb6f779796090ab1e72934e0d1ca860dee78',
  artifactPublicKeyBase64URL: '15MaLfvECwoagY8Oehclhk5nqsngGq0ECrKkRwOxDAQ',
  eligibilityPublicKeyBase64URL: 'loXs2jEP4Spq1FRzy0dnU6qm9o6eELAuhJ7AdNw1SVE',
  checksums: Object.freeze({
    asset: 'oauthapi-llm-checksums.txt',
    sha256: '56de780fa3cbe1377a7259ac6b27a10fcf4870df87d5c60df80b059e10097240',
  }),
  platforms: platformAssets,
})

export const publicAccountBridgePlatforms = Object.freeze([
  'arm64-darwin',
  'x64-darwin',
  'x64-win32',
])

export function accountBridgeReleaseAssetUrl(asset) {
  return `${accountBridgeReleasePins.repository}/releases/download/${accountBridgeReleasePins.tag}/${asset}`
}
