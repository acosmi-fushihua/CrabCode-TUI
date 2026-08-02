/**
 * Tool-internal prompt constants for the media-generation tool
 * These live inside the tool module and are not part of the system-prompt
 * registry. No hardcoded model ids: selection is fully catalog-driven.
 */

export const MEDIA_GENERATION_TOOL_NAME = 'GenerateMedia'

export function getMediaGenerationPrompt(): string {
  return `Generate an image or a video from a text prompt using a managed generation model. When the user asks you to generate/create/draw an image ("生成/画一张图") or generate a video ("生成/做一段视频"), use this tool — do not substitute a textual description for the actual generation.

- Set \`mediaType: "image"\` to generate a still image (synchronous; returns once the image is saved).
- Set \`mediaType: "video"\` to generate a video (asynchronous; the tool submits the job and polls until it completes — this can take a few minutes).
- \`prompt\` (required): a clear, detailed description of the media to generate. Be specific about subject, style, composition, and mood.

Image-only options:
- \`width\` / \`height\`: output dimensions in pixels (defaults are model-defined, typically 1024×1024).
- \`style\`: an optional style hint (e.g. "photographic", "anime", "watercolor").

Video-only options:
- \`resolution\`: an optional resolution hint (e.g. "720p", "1080p").
- \`duration\`: desired length in seconds. This is also reported for per-duration billing.

Clarify-before-generate (REQUIRED — every call is billed for real):
- If the request is ambiguous — an image request without a clear subject, a video request without a clear subject, or a request open to multiple reasonable interpretations — you MUST first ask the user ONE short clarifying question (listing your candidate interpretations) and wait for their answer before calling this tool. Never guess and generate.
- If the request is specific enough, generate directly without asking.

Anti-abuse rules:
- At most ONE generation call per turn, unless the user explicitly asked for multiple images/clips.
- If a call fails (no capable model, entitlement/quota error, upstream failure), report the error to the user and STOP — do NOT retry with the same or trivially-adjusted parameters. These failures are terminal for the session; retrying only repeats the failure.

Notes:
- The generated file is represented by a media artifact card. After a successful generation, ALWAYS tell the user: the safe artifact display name, which model generated it, and the billing dimension (images are billed per image; videos are billed per duration). Never expose the internal cache path or upstream source URL in conversational text.
- This tool is only available when an appropriate generation model is enabled in the account catalog. If no capable model exists, the tool reports that plainly — relay the message (including the purchase guidance) to the user and do not retry.`
}
