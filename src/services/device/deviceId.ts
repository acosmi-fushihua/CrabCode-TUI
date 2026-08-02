// W-DEVICE-ACCOUNT-CAP 2026-06-29 — 稳定设备标识。
//
// 每设备账号上限（每设备 ≤2 账号）需要一个**稳定、跨进程共享**的
// device-id。存 `~/.crabcode/device-id`（共享 config 目录，三种运行时都读同一文件）
// 而非 OS keychain：device-id 不是机密（只是标识符），keychain 在不同运行时
// worker 两种运行时间不一定可共享，而 config 文件三者都稳定可读（§4 config 目录契约）。
// 安全最终靠网关权威绑定，本地标识只是上报载体。

import { randomUUID } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'

import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'
import { logError } from '../../utils/log.js'

function deviceIdPath(): string {
  return join(getCrabCodeConfigHomeDir(), 'device-id')
}

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

/**
 * 读取（不存在则生成并持久化）本机稳定 device-id。
 *
 * 幂等：首次生成一个 UUID 写盘（0600），后续读回同一值。写盘失败（只读 FS 等）
 * fail-soft —— 返回内存里刚生成的值，本次登录仍能上报，只是下次重生成（设备上限
 * 退化为「按会话」而非「按设备」，但网关侧仍按收到的 id 计数，不崩）。
 */
export function getDeviceId(): string {
  const path = deviceIdPath()
  try {
    if (existsSync(path)) {
      const existing = readFileSync(path, 'utf8').trim()
      if (UUID_RE.test(existing)) {
        return existing
      }
    }
  } catch (error) {
    logError(error as Error)
  }

  const id = randomUUID()
  try {
    mkdirSync(dirname(path), { recursive: true })
    writeFileSync(path, `${id}\n`, { mode: 0o600 })
  } catch (error) {
    logError(error as Error)
  }
  return id
}
