// Auto-generated stub for filePersistence types
export const DEFAULT_UPLOAD_CONCURRENCY = 5;
export const FILE_COUNT_LIMIT = 1000;
export const OUTPUTS_SUBDIR = 'outputs';

export interface PersistedFile { filename: string; file_id: string; path?: string; size?: number; }
export interface FailedPersistence { filename: string; path?: string; error: string; }
export interface FilesPersistedEventData { files: PersistedFile[]; failed: FailedPersistence[]; }
export type TurnStartTime = number;
