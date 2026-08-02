import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import {
  chatComplete,
  chatStreamAdapter,
} from '../../src/services/acosmi/client.js'
import {
  isAccountBridgeReference,
  parseAccountBridgeReference,
  renderAccountBridgeReference,
} from '../../src/utils/model/accountBridgeReference.js'
import { isNonGatewayModelReference } from '../../src/utils/model/nonGatewayModelReference.js'
import { sideQuery } from '../../src/utils/sideQuery.js'
import type { BetaMessage } from '../../src/types/api-types.js'

const ROUTE_ID = 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
const ACCOUNT_REFERENCE = `account:${ROUTE_ID}`

describe('Account Bridge opaque runtime reference', () => {
  test('round-trips the fixed HMAC-SHA256 base64url shape', () => {
    expect(ROUTE_ID).toHaveLength(43)
    expect(parseAccountBridgeReference(ACCOUNT_REFERENCE)).toBe(ROUTE_ID)
    expect(renderAccountBridgeReference(ROUTE_ID)).toBe(ACCOUNT_REFERENCE)
    expect(isAccountBridgeReference(` ${ACCOUNT_REFERENCE} `)).toBe(true)
  })

  test('rejects malformed and identity-bearing route values', () => {
    for (const invalid of [
      '',
      'account:',
      'Account:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA',
      'account:person@example.com',
      'account:provider/model',
      'account:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
    ]) {
      expect(parseAccountBridgeReference(invalid)).toBeNull()
      expect(isAccountBridgeReference(invalid)).toBe(false)
    }
    expect(() => renderAccountBridgeReference('person@example.com')).toThrow(
      /opaque.*base64url/,
    )
  })

  test('classifies malformed reserved references fail-closed', () => {
    expect(isNonGatewayModelReference(ACCOUNT_REFERENCE)).toBe(true)
    expect(isNonGatewayModelReference('account:')).toBe(true)
    expect(isNonGatewayModelReference('custom:missing')).toBe(true)
    expect(isNonGatewayModelReference('local:missing')).toBe(true)
    expect(isNonGatewayModelReference('managed-model')).toBe(false)
  })
})

describe('Account Bridge gateway hard wall', () => {
  test('SDK complete and stream entry points reject before client creation', async () => {
    await expect(
      chatComplete(ACCOUNT_REFERENCE, {
        max_tokens: 1,
        rawMessages: [{ role: 'user', content: 'private account content' }],
      } as never),
    ).rejects.toThrow(/non-gateway|refusing/i)
    expect(() =>
      chatStreamAdapter(ACCOUNT_REFERENCE, { max_tokens: 1 } as never),
    ).toThrow(/non-gateway|refusing/i)
    expect(() =>
      chatStreamAdapter('account:', { max_tokens: 1 } as never),
    ).toThrow(/non-gateway|refusing/i)
  })

  test('side queries return empty without a gateway call', async () => {
    let gatewayCalls = 0
    const result = await sideQuery(
      {
        model: ACCOUNT_REFERENCE,
        system: 'system',
        messages: [{ role: 'user', content: 'private account content' }],
        querySource: 'sdk',
      } as never,
      {
        chatComplete: (async () => {
          gatewayCalls++
          throw new Error('must not be called')
        }) as never,
        systemToString: (() => 'system') as never,
      },
    )
    expect(gatewayCalls).toBe(0)
    expect((result as BetaMessage).content).toEqual([])
  })

  test('query routing and every fallback use the shared guard', () => {
    const source = readFileSync(
      join(import.meta.dir, '../../src/services/api/queryModel.ts'),
      'utf8',
    )
    expect(source).toMatch(
      /if \(isNonGatewayModelReference\(params\.model\)\) \{\s*throw new DanglingModelReferenceError\(params\.model\)/,
    )
    expect(source).toMatch(
      /if \(isNonGatewayModelReference\(retryParams\.model\)\) \{\s*throw new DanglingModelReferenceError\(retryParams\.model\)/,
    )
    expect(source).toMatch(
      /is404StreamCreationError &&\s*!isNonGatewayModelReference\(options\.model\)/,
    )
  })
})
