export type SelectionPoint = {
  line: number
  character: number
}

export type SelectionData = {
  selection: {
    start: SelectionPoint
    end: SelectionPoint
  } | null
  text?: string
  filePath?: string
}

export type IDESelection = {
  lineCount: number
  lineStart?: number
  text?: string
  filePath?: string
}
