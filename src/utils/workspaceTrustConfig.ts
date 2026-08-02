import { resolve } from 'node:path'

import {
  normalizeWorkspaceProjectKey,
  resolveWorkspaceProjectKey,
} from './workspaceProjectIdentity.js'

type WorkspaceProjectRecord = Record<string, unknown> & {
  hasTrustDialogAccepted?: unknown
}

export type WorkspaceTrustProjects =
  | Record<string, WorkspaceProjectRecord>
  | undefined

export function isInitialWorkspaceTrustedInProjects(
  projects: WorkspaceTrustProjects,
  input: {
    originalCwd: string
    currentCwd: string
    sessionTrusted: boolean
  },
): boolean {
  if (input.sessionTrusted) return true
  if (projectIsTrusted(projects, resolveWorkspaceProjectKey(input.originalCwd))) {
    return true
  }
  return isPathTrustedInProjects(projects, input.currentCwd)
}

export function isPathTrustedInProjects(
  projects: WorkspaceTrustProjects,
  dir: string,
): boolean {
  let currentPath = normalizeWorkspaceProjectKey(resolve(dir))
  while (true) {
    if (projectIsTrusted(projects, currentPath)) return true
    const parentPath = normalizeWorkspaceProjectKey(resolve(currentPath, '..'))
    if (parentPath === currentPath) return false
    currentPath = parentPath
  }
}

export function isWorkspaceTrustedInProjects(
  projects: WorkspaceTrustProjects,
  dir: string,
): boolean {
  if (projectIsTrusted(projects, resolveWorkspaceProjectKey(dir))) {
    return true
  }
  return isPathTrustedInProjects(projects, dir)
}

export { resolveWorkspaceProjectKey }

function projectIsTrusted(
  projects: WorkspaceTrustProjects,
  projectKey: string,
): boolean {
  const project = projects?.[projectKey]
  return (
    project !== null &&
    typeof project === 'object' &&
    !Array.isArray(project) &&
    project.hasTrustDialogAccepted === true
  )
}
