/**
 * Stubbed-out auth-status manager.
 *
 * Previously managed cloud-provider authentication status (AWS Bedrock,
 * GCP Vertex). Since providers.ts now always returns 'firstParty', the
 * real implementation is dead code. Consumers still import this module,
 * so it is kept as a no-op stub to avoid cascading breakage.
 */

export type AwsAuthStatus = {
  isAuthenticating: boolean
  output: string[]
  error?: string
}

const IDLE_STATUS: AwsAuthStatus = Object.freeze({
  isAuthenticating: false,
  output: [],
})

export class AwsAuthStatusManager {
  private static instance: AwsAuthStatusManager | null = null

  static getInstance(): AwsAuthStatusManager {
    if (!AwsAuthStatusManager.instance) {
      AwsAuthStatusManager.instance = new AwsAuthStatusManager()
    }
    return AwsAuthStatusManager.instance
  }

  getStatus(): AwsAuthStatus {
    return { ...IDLE_STATUS, output: [] }
  }

  startAuthentication(): void { /* no-op */ }
  addOutput(_line: string): void { /* no-op */ }
  setError(_error: string): void { /* no-op */ }
  endAuthentication(_success: boolean): void { /* no-op */ }

  subscribe(_cb: (status: AwsAuthStatus) => void): () => void {
    // Never fires; return a no-op unsubscribe
    return () => {}
  }

  static reset(): void {
    AwsAuthStatusManager.instance = null
  }
}
