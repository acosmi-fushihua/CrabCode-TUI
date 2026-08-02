import { isEnvTruthy } from '../../utils/envUtils.js'

type StreamOutputPolicyInput = Readonly<{
  directQueryEventDelivery: boolean
  streamlinedFeatureEnabled: boolean
  streamlinedEnvironmentValue: string | undefined
  outputFormat: string | undefined
}>

type SdkHookEventPolicyInput = Readonly<{
  directQueryEventDelivery: boolean
  outputFormat: string | undefined
  verbose: boolean | undefined
}>

/**
 * The native TUI consumes original query() objects. Streamlined projection is
 * a print/SDK presentation feature and must never rewrite that renderer-owned
 * lifecycle, even when the process inherits the opt-in environment variable.
 */
export function shouldTransformStreamOutput(
  input: StreamOutputPolicyInput,
): boolean {
  return (
    !input.directQueryEventDelivery &&
    input.streamlinedFeatureEnabled &&
    isEnvTruthy(input.streamlinedEnvironmentValue) &&
    input.outputFormat === 'stream-json'
  )
}

/**
 * Hook lifecycle envelopes belong to the ordinary verbose SDK/print stream.
 * The native direct TUI observes QueryEngine messages separately and must not
 * acquire a second path that exposes hook stdout or additional context.
 */
export function shouldRegisterSdkHookEventHandler(
  input: SdkHookEventPolicyInput,
): boolean {
  return (
    !input.directQueryEventDelivery &&
    input.outputFormat === 'stream-json' &&
    input.verbose === true
  )
}
