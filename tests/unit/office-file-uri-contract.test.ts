import { describe, expect, test } from 'bun:test'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

import { convertOfficeToPdf } from '../../src/utils/officeParse/libreoffice.js'

describe('LibreOffice profile URL', () => {
  test('uses a canonical file URL for the isolated profile', async () => {
    const outDir = process.platform === 'win32'
      ? 'C:\\Users\\u a\\AppData\\Local\\Temp\\crabcode-office'
      : '/tmp/crabcode office'
    let args: string[] = []

    await convertOfficeToPdf(join(outDir, 'deck.pptx'), '/bin/soffice', {
      makeOutDir: async () => outDir,
      exec: async request => {
        args = request.args
        return { code: 0, stdout: '', stderr: '' }
      },
      exists: async () => false,
    })

    expect(args).toContain(
      `-env:UserInstallation=${pathToFileURL(join(outDir, '.profile')).href}`,
    )
  })
})
