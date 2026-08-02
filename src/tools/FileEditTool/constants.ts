// In its own file to avoid circular dependencies
export const FILE_EDIT_TOOL_NAME = 'Edit'

// Permission pattern for granting session-level access to the project's .crabcode/ folder
export const CRABCODE_FOLDER_PERMISSION_PATTERN = '/.crabcode/**'

// Permission pattern for granting session-level access to the global ~/.crabcode/ folder
export const GLOBAL_CRABCODE_FOLDER_PERMISSION_PATTERN = '~/.crabcode/**'

export const FILE_UNEXPECTEDLY_MODIFIED_ERROR =
  'File has been unexpectedly modified. Read it again before attempting to write it.'
