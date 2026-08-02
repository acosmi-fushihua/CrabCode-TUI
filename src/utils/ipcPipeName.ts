/**
 * Windows pipe username encoding shared by process-owned IPC clients. It
 * mirrors `acosmi-daemon-launcher/src/paths.rs::sanitize_pipe_user_name`.
 */
export function sanitizePipeUserName(raw: string): string {
  const mapped = raw.replace(/[^A-Za-z0-9._-]/g, '_')
  if (mapped === raw) return raw
  return `${mapped}-${fnv1a32Hex(raw)}`
}

export function fnv1a32Hex(raw: string): string {
  const bytes = Buffer.from(raw, 'utf8')
  let hash = 0x811c9dc5
  for (const byte of bytes) {
    hash ^= byte
    hash = Math.imul(hash, 0x01000193) >>> 0
  }
  return hash.toString(16).padStart(8, '0')
}
