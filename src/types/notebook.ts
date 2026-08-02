/**
 * TypeScript types for Jupyter notebook (.ipynb) file format.
 */

/** Processed output image extracted from a notebook cell. */
export type NotebookOutputImage = {
  image_data: string
  media_type: 'image/png' | 'image/jpeg'
}

/** A processed/transformed version of a cell's output used in tool results. */
export type NotebookCellSourceOutput = {
  output_type: string
  text?: string
  image?: NotebookOutputImage
}

/** A processed representation of a notebook cell with source and outputs. */
export type NotebookCellSource = {
  cellType: 'code' | 'markdown' | 'raw'
  source: string
  execution_count?: number
  cell_id: string
  language?: string
  outputs?: NotebookCellSourceOutput[]
}

/** Raw output block in a Jupyter notebook cell. */
export type NotebookCellOutput =
  | {
      output_type: 'stream'
      text?: string | string[]
    }
  | {
      output_type: 'execute_result' | 'display_data'
      data?: Record<string, unknown>
    }
  | {
      output_type: 'error'
      ename: string
      evalue: string
      traceback: string[]
    }

/** The cell type for a Jupyter notebook cell. */
export type NotebookCellType = 'code' | 'markdown' | 'raw'

/** A raw cell in a Jupyter notebook. */
export type NotebookCell = {
  id?: string
  cell_type: 'code' | 'markdown' | 'raw'
  source: string | string[]
  execution_count?: number | null
  outputs?: NotebookCellOutput[]
  metadata?: Record<string, unknown>
}

/** Top-level structure of a Jupyter notebook .ipynb file. */
export type NotebookContent = {
  cells: NotebookCell[]
  metadata: {
    language_info?: {
      name?: string
    }
    kernelspec?: Record<string, unknown>
    [key: string]: unknown
  }
  nbformat: number
  nbformat_minor: number
}
