import { t } from '../../i18n/index.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import {
  calculateShouldShowGrove,
  getGroveNoticeConfig,
  getGroveSettings,
  isQualifiedForGrove,
  markGroveNoticeViewed,
  updateGroveSettings,
} from './grove.js'

type GroveDecision =
  | 'accept_opt_in'
  | 'accept_opt_out'
  | 'defer'
  | 'escape'

type GroveOption = {
  decision: Exclude<GroveDecision, 'escape'>
  label: string
}

export type DirectTuiGroveRenderer = {
  handleNativeTuiGroveTerms(input: {
    title: string
    body: string[]
    links: Array<{ label: string; url: string }>
    options: GroveOption[]
    dismissable: boolean
  }): Promise<{ decision: GroveDecision }>
}

function buildBody(
  noticeIsGracePeriod: boolean,
  effectiveDate: string | null | undefined,
): string[] {
  if (noticeIsGracePeriod) {
    return [
      ...(effectiveDate
        ? [t('grove_grace_period_notice', { effectiveDate })]
        : []),
      t('grove_whats_changing'),
      `${t('grove_help_improve')}: ${t('grove_allow_chats_help')}.`,
      t('grove_data_retention_updates'),
      t('grove_select_continue'),
      t('grove_choice_immediate'),
    ]
  }
  return [
    t('grove_terms_updated'),
    t('grove_whats_changing'),
    `${t('grove_help_improve')}: ${t('grove_allow_chats_help_post')}`,
    `${t('grove_data_retention_how_affects')}: ${t('grove_data_retention_body')}`,
    t('grove_select_continue'),
    t('grove_choice_immediate'),
  ]
}

/**
 * Run the existing Grove policy authority through the private native-TUI
 * renderer. This adapter owns no eligibility or persistence policy.
 *
 * Returns false only for the established post-grace Escape behavior, where
 * the interactive session must exit without starting a model turn.
 */
export async function runDirectTuiGroveTermsBarrier(
  renderer: DirectTuiGroveRenderer,
  location: 'onboarding' | 'policy_update_modal' = 'policy_update_modal',
): Promise<boolean> {
  if (!(await isQualifiedForGrove())) return true

  const [settingsResult, configResult] = await Promise.all([
    getGroveSettings(),
    getGroveNoticeConfig(),
  ])
  const config = configResult.success ? configResult.data : null
  if (
    !calculateShouldShowGrove(settingsResult, configResult, false) ||
    config === null
  ) {
    return true
  }

  const options: GroveOption[] = config.domain_excluded
    ? [
        {
          decision: 'accept_opt_out',
          label: t('grove_accept_opt_out_domain'),
        },
      ]
    : [
        {
          decision: 'accept_opt_in',
          label: t('grove_accept_opt_in'),
        },
        {
          decision: 'accept_opt_out',
          label: t('grove_accept_opt_out'),
        },
      ]
  if (config.notice_is_grace_period) {
    options.push({
      decision: 'defer',
      label: t('grove_not_now'),
    })
  }

  await markGroveNoticeViewed()
  logEvent('tengu_grove_policy_viewed', {
    location:
      location as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    dismissable:
      config.notice_is_grace_period as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  })

  const { decision } = await renderer.handleNativeTuiGroveTerms({
    title: t('grove_dialog_title'),
    body: buildBody(
      config.notice_is_grace_period,
      config.effective_date,
    ),
    links: [
      {
        label: t('grove_learn_more'),
        url: 'https://acosmi.com/zh/news/updates-to-our-consumer-terms',
      },
      {
        label: t('grove_or_read_updated'),
        url: 'https://acosmi.com/zh/legal/terms',
      },
      {
        label: t('grove_and_privacy_policy'),
        url: 'https://acosmi.com/zh/legal/privacy',
      },
      {
        label: t('grove_review_privacy'),
        url: 'https://acosmi.com/settings/data-privacy-controls',
      },
    ],
    options,
    dismissable: config.notice_is_grace_period,
  })

  const allowed = new Set<GroveDecision>(options.map(option => option.decision))
  if (!config.notice_is_grace_period) allowed.add('escape')
  if (!allowed.has(decision)) {
    throw new Error(
      `native TUI returned an inadmissible Grove decision: ${decision}`,
    )
  }

  switch (decision) {
    case 'accept_opt_in':
      await updateGroveSettings(true)
      logEvent('tengu_grove_policy_submitted', {
        state: true,
        dismissable:
          config.notice_is_grace_period as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
      return true
    case 'accept_opt_out':
      await updateGroveSettings(false)
      logEvent('tengu_grove_policy_submitted', {
        state: false,
        dismissable:
          config.notice_is_grace_period as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      })
      return true
    case 'defer':
      logEvent('tengu_grove_policy_dismissed', { state: true })
      return true
    case 'escape':
      logEvent('tengu_grove_policy_escaped', {})
      return false
  }
}
