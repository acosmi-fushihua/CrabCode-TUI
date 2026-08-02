/**
 * Ambient module declarations for optional/native packages that are
 * dynamically imported and may not ship with bundled type definitions.
 *
 * These stubs provide just enough typing for the call-sites in this codebase.
 */

// ---------------------------------------------------------------------------
// diff
// @types/diff@7.0.2 uses index.d.mts → index.js re-export chain. With
// moduleResolution: bundler this chain fails to resolve in ambient context.
// We provide a complete override that mirrors @types/diff@7.0.2 plus adds
// the StructuredPatchHunk alias used throughout this codebase.
// ---------------------------------------------------------------------------
declare module 'diff' {
  export interface StructuredPatchHunk {
    oldStart: number
    oldLines: number
    newStart: number
    newLines: number
    lines: string[]
  }

  /** Canonical name in @types/diff; alias kept for new code. */
  export type Hunk = StructuredPatchHunk

  export interface Change {
    value: string
    added?: boolean
    removed?: boolean
    count?: number
  }

  export interface ArrayChange<T> {
    value: T[]
    added?: boolean
    removed?: boolean
    count?: number
  }

  export interface ParsedDiff {
    oldFileName?: string
    newFileName?: string
    oldHeader?: string
    newHeader?: string
    hunks: StructuredPatchHunk[]
  }

  export interface BaseOptions {
    timeout?: number
    maxEditLength?: number
    oneChangePerToken?: boolean
  }

  export interface LinesOptions extends BaseOptions {
    newlineIsToken?: boolean
    ignoreWhitespace?: boolean
    stripTrailingCr?: boolean
    ignoreNewlineAtEof?: boolean
  }

  export interface WordsOptions extends BaseOptions {
    intlSegmenter?: unknown
    unicodeWordSegmentation?: boolean
    ignoreCase?: boolean
  }

  export interface PatchOptions extends LinesOptions {
    context?: number
  }

  export interface ApplyPatchOptions {
    fuzzFactor?: number
    compareLine?: (lineNumber: number, line: string, operation: string, patchContent: string) => boolean
  }

  export function diffChars(oldStr: string, newStr: string, options?: BaseOptions): Change[]
  export function diffWords(oldStr: string, newStr: string, options?: WordsOptions): Change[]
  export function diffWordsWithSpace(oldStr: string, newStr: string, options?: WordsOptions): Change[]
  export function diffLines(oldStr: string, newStr: string, options?: LinesOptions): Change[]
  export function diffTrimmedLines(oldStr: string, newStr: string, options?: LinesOptions): Change[]
  export function diffSentences(oldStr: string, newStr: string, options?: BaseOptions): Change[]
  export function diffCss(oldStr: string, newStr: string, options?: BaseOptions): Change[]
  export function diffJson(oldObj: object | string, newObj: object | string, options?: LinesOptions): Change[]
  export function diffArrays<T>(oldArr: T[], newArr: T[], options?: BaseOptions): ArrayChange<T>[]
  export function structuredPatch(
    oldFileName: string,
    newFileName: string,
    oldStr: string,
    newStr: string,
    oldHeader?: string,
    newHeader?: string,
    options?: PatchOptions,
  ): ParsedDiff
  export function createPatch(
    fileName: string,
    oldStr: string,
    newStr: string,
    oldHeader?: string,
    newHeader?: string,
    options?: PatchOptions,
  ): string
  export function createTwoFilesPatch(
    oldFileName: string,
    newFileName: string,
    oldStr: string,
    newStr: string,
    oldHeader?: string,
    newHeader?: string,
    options?: PatchOptions,
  ): string
  export function parsePatch(uniDiff: string): ParsedDiff[]
  export function applyPatch(
    source: string,
    patch: string | ParsedDiff | ParsedDiff[],
    options?: ApplyPatchOptions,
  ): string | false
  export function applyPatches(patch: string | ParsedDiff[], options: ApplyPatchOptions & {
    loadFile: (index: ParsedDiff, callback: (err: unknown, data: string) => void) => void
    patched: (index: ParsedDiff, content: string | false, callback: (err: unknown) => void) => void
    complete: (err: unknown) => void
  }): void
  export function convertChangesToDMP(changes: Change[]): Array<[number, string]>
  export function convertChangesToXML(changes: Change[]): string
  export function merge(mine: string, theirs: string, base: string): string
}

// ---------------------------------------------------------------------------
// image-processor-napi
// Native macOS clipboard image reader (NSPasteboard + CoreGraphics).
// Used in src/utils/imagePaste.ts for fast clipboard image access.
// ---------------------------------------------------------------------------
declare module 'image-processor-napi' {
  export interface NativeClipboardModule {
    /** Returns true when the clipboard contains an image. */
    hasClipboardImage?(): boolean

    /**
     * Reads an image from the clipboard, downsampling to at most
     * maxWidth × maxHeight pixels via CoreGraphics.
     * Returns null when the clipboard contains no image.
     */
    readClipboardImage?(
      maxWidth: number,
      maxHeight: number,
    ): { png: Buffer } | null
  }

  /**
   * Returns the native module instance, or undefined/null when the
   * native addon is not available (non-macOS, arm mismatch, etc.).
   */
  export function getNativeModule(): NativeClipboardModule | null | undefined

  // Sharp-compatible image processing function (when available)
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  export const sharp: ((...args: any[]) => any) | undefined

  // Default export may also be a sharp-compatible function
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  export default function(...args: any[]): any
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-metrics-otlp-grpc
// OTLP metric exporter over gRPC.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-metrics-otlp-grpc' {
  export class OTLPMetricExporter {
    constructor(config?: Record<string, unknown>)
    export(metrics: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-metrics-otlp-http
// OTLP metric exporter over HTTP/JSON.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-metrics-otlp-http' {
  export class OTLPMetricExporter {
    constructor(config?: Record<string, unknown>)
    export(metrics: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-metrics-otlp-proto
// OTLP metric exporter over HTTP/protobuf.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-metrics-otlp-proto' {
  export class OTLPMetricExporter {
    constructor(config?: Record<string, unknown>)
    export(metrics: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-logs-otlp-grpc
// OTLP log exporter over gRPC.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-logs-otlp-grpc' {
  export class OTLPLogExporter {
    constructor(config?: Record<string, unknown>)
    export(logs: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-logs-otlp-http
// OTLP log exporter over HTTP/JSON.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-logs-otlp-http' {
  export class OTLPLogExporter {
    constructor(config?: Record<string, unknown>)
    export(logs: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-logs-otlp-proto
// OTLP log exporter over HTTP/protobuf.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-logs-otlp-proto' {
  export class OTLPLogExporter {
    constructor(config?: Record<string, unknown>)
    export(logs: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-trace-otlp-grpc
// OTLP trace/span exporter over gRPC.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-trace-otlp-grpc' {
  export class OTLPTraceExporter {
    constructor(config?: Record<string, unknown>)
    export(spans: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-trace-otlp-http
// OTLP trace/span exporter over HTTP/JSON.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-trace-otlp-http' {
  export class OTLPTraceExporter {
    constructor(config?: Record<string, unknown>)
    export(spans: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-trace-otlp-proto
// OTLP trace/span exporter over HTTP/protobuf.
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-trace-otlp-proto' {
  export class OTLPTraceExporter {
    constructor(config?: Record<string, unknown>)
    export(spans: unknown, resultCallback: (result: unknown) => void): void
    shutdown(): Promise<void>
  }
}

// ---------------------------------------------------------------------------
// @opentelemetry/exporter-prometheus
// Prometheus metric exporter (pull-based).
// ---------------------------------------------------------------------------
declare module '@opentelemetry/exporter-prometheus' {
  export class PrometheusExporter {
    constructor(config?: Record<string, unknown>)
    startServer?(): void
    shutdown(): Promise<void>
  }
}
