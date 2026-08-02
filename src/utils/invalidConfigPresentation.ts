import type { ConfigParseError } from './errors.js'

type InvalidConfigPresenter = (error: ConfigParseError) => Promise<void>

let presenter: InvalidConfigPresenter | undefined

export function installInvalidConfigPresenter(
  nextPresenter: InvalidConfigPresenter,
): void {
  presenter = nextPresenter
}

export async function presentInvalidConfigIfAvailable(
  error: ConfigParseError,
): Promise<boolean> {
  if (!presenter) return false
  await presenter(error)
  return true
}
