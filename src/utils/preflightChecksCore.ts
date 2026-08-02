import axios from 'axios'

import { getOauthConfig } from '../constants/oauth.js'
import { getSSLErrorHint } from '../services/api/errorUtils.js'
import { logEvent } from '../services/analytics/index.js'
import { getUserAgent } from './http.js'
import { logError } from './log.js'

export interface PreflightCheckResult {
  success: boolean
  error?: string
  sslHint?: string
}

/**
 * Renderer-independent onboarding connectivity check.
 *
 * Keep this as the single authority for both the legacy Ink screen and the
 * native direct TUI. A failed check is information for the onboarding stage;
 * the caller retains the historical non-blocking continuation policy.
 */
export async function checkOnboardingEndpoints(): Promise<PreflightCheckResult> {
  try {
    const oauthConfig = getOauthConfig()
    const tokenUrl = new URL(oauthConfig.TOKEN_URL)
    const endpoints = [
      `${oauthConfig.BASE_API_URL}/api/hello`,
      `${tokenUrl.origin}/v1/oauth/hello`,
    ]
    const checkEndpoint = async (
      url: string,
    ): Promise<PreflightCheckResult> => {
      try {
        await axios.get(url, {
          headers: { 'User-Agent': getUserAgent() },
          // This step answers only "can the terminal reach the configured
          // Acosmi HTTPS origin?". 401/403 prove DNS, TLS and HTTP
          // connectivity just as conclusively as 200, while redirects are a
          // normal part of the OAuth surface. Let the real login flow remain
          // the authority for authentication and endpoint semantics.
          validateStatus: status => status >= 100 && status <= 599,
        })
        return { success: true }
      } catch (error) {
        // Some Axios adapters can still reject an HTTP response despite a
        // caller-supplied validateStatus. A received response is connectivity
        // evidence; only failures without a response are network failures.
        if (axios.isAxiosError(error) && error.response) {
          return { success: true }
        }
        const sslHint = getSSLErrorHint(error)
        return {
          success: false,
          error: `Failed to connect to ${new URL(url).hostname}: ${
            error instanceof Error
              ? (error as ErrnoException).code || error.message
              : String(error)
          }`,
          sslHint: sslHint ?? undefined,
        }
      }
    }

    const results = await Promise.all(endpoints.map(checkEndpoint))
    const failedResult = results.find(result => !result.success)
    if (failedResult) {
      logEvent('tengu_preflight_check_failed', {
        isConnectivityError: true,
        hasErrorMessage: !!failedResult.error,
        isSSLError: !!failedResult.sslHint,
      })
    }
    return failedResult ?? { success: true }
  } catch (error) {
    logError(error as Error)
    logEvent('tengu_preflight_check_failed', {
      isConnectivityError: true,
    })
    return {
      success: false,
      error: `Connectivity check error: ${
        error instanceof Error
          ? (error as ErrnoException).code || error.message
          : String(error)
      }`,
    }
  }
}
