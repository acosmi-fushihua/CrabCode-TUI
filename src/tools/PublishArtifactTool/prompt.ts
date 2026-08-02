export const DESCRIPTION =
  'Register an existing local file as a conversation artifact for inline display'

export const PROMPT = `Use this tool after another tool or command has successfully created a file that the user should see in the conversation.

- Pass the file's absolute runtime path. This tool does not create, move, or modify files.
- Declare the actual artifact kind and MIME type. The host independently validates existence, size, MIME, and blocked active-content formats before displaying anything.
- Call this tool explicitly instead of relying on a path printed in shell output or assistant text.
- Submit one artifact per call. If host validation rejects the candidate, correct the path or metadata; do not repeatedly submit the same invalid candidate.`
