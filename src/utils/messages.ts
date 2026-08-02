/**
 * messages.ts -- Backward-compatible re-export.
 *
 * This file was the original 5512-line monolith. It has been split into:
 *   - messages/envelope.ts   (sentinel constants, predicates, ID derivation)
 *   - messages/factory.ts    (message creation factories)
 *   - messages/normalize.ts  (API normalization, stream handling)
 *   - messages/render.ts     (TS-only UI helpers)
 *   - messages/index.ts      (barrel re-export)
 *
 * All exports are re-exported from messages/index.ts so that the 79 files
 * importing from 'src/utils/messages.js' continue to work unchanged.
 */
export * from './messages/index.js'
