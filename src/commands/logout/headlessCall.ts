import { t } from '../../i18n/index.js'
import { performLogout } from '../../services/auth/logout.js'
import type { LocalCommandCall } from '../../types/command.js'
import { gracefulShutdownSync } from '../../utils/gracefulShutdown.js'

export const call: LocalCommandCall = async () => {
  await performLogout({ clearOnboarding: true })
  setTimeout(() => {
    gracefulShutdownSync(0, 'logout')
  }, 200)
  return { type: 'text', value: t('logout_success') }
}
