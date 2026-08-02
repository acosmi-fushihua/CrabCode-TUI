export const MEMORY_RECOVERY_SCHEMA_VERSION = 1 as const

export type MemoryRecoveryLocator = {
  recovery_schema_version: number
  trigger_id: string
  kind: 'dream' | 'extract'
  session_id: string
  current_session_id: string
  context_leaf_uuid: string
  project_cwd: string
  transcript_path: string
  project_state_dir: string
  memory_dir: string
}
