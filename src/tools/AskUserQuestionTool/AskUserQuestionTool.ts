import { feature } from '../../utils/featurePolyfill.js'
import {
  getAllowedChannels,
  getQuestionPreviewFormat,
} from 'src/bootstrap/state.js'
import { z } from 'zod/v4'
import type { Tool } from '../../Tool.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { createToolPresentationDelegates } from '../toolPresentationRegistry.js'
import {
  ASK_USER_QUESTION_TOOL_CHIP_WIDTH,
  ASK_USER_QUESTION_TOOL_NAME,
  ASK_USER_QUESTION_TOOL_PROMPT,
  DESCRIPTION,
  PREVIEW_FEATURE_PROMPT,
} from './prompt.js'

const MAX_IDENTIFIER_CHARS = 128
const MAX_QUESTION_CHARS = 10_000
const MAX_LABEL_CHARS = 200
const MAX_DESCRIPTION_CHARS = 10_000
const MAX_PREVIEW_CHARS = 100_000
const MAX_PREVIEW_BYTES = 100_000
const MAX_CUSTOM_TEXT_BYTES = 20_000
const MAX_NOTES_BYTES = 20_000
const MAX_LEGACY_ANSWER_BYTES =
  MAX_CUSTOM_TEXT_BYTES + 4 * MAX_LABEL_CHARS * 4 + 16
const MAX_REASON_CHARS = 2_000
const UTF8_ENCODER = new TextEncoder()

const identifierSchema = lazySchema(() =>
  z
    .string()
    .min(1)
    .max(MAX_IDENTIFIER_CHARS)
    .regex(
      /^[A-Za-z0-9][A-Za-z0-9._:-]*$/,
      'IDs must start with an ASCII letter or number and contain only letters, numbers, dot, underscore, colon, or hyphen',
    ),
)

const questionOptionSchema = lazySchema(() =>
  z.strictObject({
    id: identifierSchema()
      .optional()
      .describe(
        'Optional stable option identifier. When omitted, CrabCode assigns one before presenting the question.',
      ),
    label: z
      .string()
      .trim()
      .min(1)
      .max(MAX_LABEL_CHARS)
      .describe(
        'The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice.',
      ),
    description: z
      .string()
      .trim()
      .min(1)
      .max(MAX_DESCRIPTION_CHARS)
      .describe(
        'Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications.',
      ),
    recommended: z
      .boolean()
      .default(false)
      .describe(
        'Visual recommendation metadata. The host may highlight this option but must never preselect it.',
      ),
    preview: z
      .string()
      .max(MAX_PREVIEW_CHARS)
      .refine(value => utf8ByteLength(value) <= MAX_PREVIEW_BYTES, {
        message: `Preview must be at most ${MAX_PREVIEW_BYTES} UTF-8 bytes`,
      })
      .optional()
      .describe(
        'Optional preview content rendered when this option is focused. Use for mockups, code snippets, or visual comparisons that help users compare options. See the tool description for the expected content format.',
      ),
  }),
)

const questionSchema = lazySchema(() =>
  z.strictObject({
    id: identifierSchema()
      .optional()
      .describe(
        'Optional stable question identifier. When omitted, CrabCode assigns one before presenting the question.',
      ),
    question: z
      .string()
      .trim()
      .min(1)
      .max(MAX_QUESTION_CHARS)
      .describe(
        'The complete question to ask the user. Should be clear and specific. If multiSelect is true, phrase it accordingly.',
      ),
    header: z
      .string()
      .trim()
      .min(1)
      .refine(value => Array.from(value).length <= ASK_USER_QUESTION_TOOL_CHIP_WIDTH, {
        message: `Header must be at most ${ASK_USER_QUESTION_TOOL_CHIP_WIDTH} Unicode characters`,
      })
      .describe(
        `Very short label displayed as a chip/tag (max ${ASK_USER_QUESTION_TOOL_CHIP_WIDTH} chars). Examples: "Auth method", "Library", "Approach".`,
      ),
    options: z
      .array(questionOptionSchema())
      .min(2)
      .max(4)
      .describe(
        "The available choices for this question. Must have 2-4 options. Each option should be distinct. Do not add an 'Other' option; the host provides custom text separately.",
      ),
    multiSelect: z
      .boolean()
      .default(false)
      .describe(
        'Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive.',
      ),
    minSelections: z
      .number()
      .int()
      .min(0)
      .max(5)
      .optional()
      .describe(
        'Multi-select only. Minimum number of choices required; custom Other text counts as one. Defaults to 1.',
      ),
    maxSelections: z
      .number()
      .int()
      .min(0)
      .max(5)
      .optional()
      .describe(
        'Multi-select only. Maximum number of choices allowed; custom Other text counts as one. Defaults to the option count plus Other.',
      ),
  }),
)

const metadataSchema = lazySchema(() =>
  z
    .strictObject({
      source: z
        .string()
        .max(200)
        .optional()
        .describe(
          'Optional identifier for the source of this question (e.g., "remember" for /remember command). Used for analytics tracking.',
        ),
    })
    .optional()
    .describe('Optional metadata for tracking and analytics purposes.'),
)

const annotationSchema = lazySchema(() =>
  z.strictObject({
    preview: z
      .string()
      .max(MAX_PREVIEW_CHARS)
      .refine(value => utf8ByteLength(value) <= MAX_PREVIEW_BYTES, {
        message: `Preview must be at most ${MAX_PREVIEW_BYTES} UTF-8 bytes`,
      })
      .optional(),
    notes: z
      .string()
      .refine(value => utf8ByteLength(value) <= MAX_NOTES_BYTES, {
        message: `Notes must be at most ${MAX_NOTES_BYTES} UTF-8 bytes`,
      })
      .optional(),
  }),
)

const annotationsSchema = lazySchema(() =>
  z
    .record(z.string(), annotationSchema())
    .optional()
    .describe(
      'Legacy per-question annotations keyed by question text. Accepted only from the trusted interaction response.',
    ),
)

const terminalStatusSchema = lazySchema(() =>
  z.enum([
    'submitted',
    'declined',
    'cancelled',
    'timed_out',
    'interaction_unavailable',
  ]),
)

const structuredAnswerSchema = lazySchema(() =>
  z.strictObject({
    questionId: identifierSchema(),
    selectedOptionIds: z.array(identifierSchema()).max(4).default([]),
    customText: z
      .string()
      .trim()
      .min(1)
      .refine(value => utf8ByteLength(value) <= MAX_CUSTOM_TEXT_BYTES, {
        message: `Custom Other text must be at most ${MAX_CUSTOM_TEXT_BYTES} UTF-8 bytes`,
      })
      .optional(),
    notes: z
      .string()
      .refine(value => utf8ByteLength(value) <= MAX_NOTES_BYTES, {
        message: `Notes must be at most ${MAX_NOTES_BYTES} UTF-8 bytes`,
      })
      .optional(),
  }),
)

const structuredResponseSchema = lazySchema(() =>
  z.strictObject({
    status: terminalStatusSchema(),
    answers: z.array(structuredAnswerSchema()).max(4).default([]),
    reason: z.string().max(MAX_REASON_CHARS).optional(),
  }),
)

const legacyAnswersSchema = lazySchema(() =>
  z.record(
    z.string(),
    z.string().refine(
      value => utf8ByteLength(value) <= MAX_LEGACY_ANSWER_BYTES,
      {
        message: `Legacy answer must be at most ${MAX_LEGACY_ANSWER_BYTES} UTF-8 bytes`,
      },
    ),
  ),
)

const answerIdsSchema = lazySchema(() =>
  z.record(z.string(), z.array(identifierSchema()).max(4)),
)

const modelInputSchema = lazySchema(() =>
  z
    .strictObject({
      questions: z
        .array(questionSchema())
        .min(1)
        .max(4)
        .describe('Questions to ask the user (1-4 questions)'),
      metadata: metadataSchema(),
    })
    .superRefine(addQuestionDefinitionIssues),
)

/**
 * Internal-only response envelope returned through a trusted permission/UI host.
 * It deliberately is not the tool's model-visible input schema: a model must
 * never be able to manufacture the user's answers or terminal status itself.
 *
 * `status`/`answer_ids` and the question-text-keyed `answers` record are a
 * transition bridge for native/legacy hosts. `response` is the canonical v2
 * representation. Hosts may dual-write both; semantic validation below requires
 * the copies to agree.
 */
const trustedUpdatedInputSchema = lazySchema(() =>
  z
    .strictObject({
      questions: z.array(questionSchema()).min(1).max(4),
      metadata: metadataSchema(),
      response: structuredResponseSchema().optional(),
      status: terminalStatusSchema().optional(),
      reason: z.string().max(MAX_REASON_CHARS).optional(),
      answers: legacyAnswersSchema().optional(),
      answer_ids: answerIdsSchema().optional(),
      annotations: annotationsSchema(),
    })
    .superRefine((data, context) => {
      addQuestionDefinitionIssues(data, context)
      const normalizedQuestions = normalizeQuestions(data.questions)
      for (const issue of trustedResponseIssues(data, normalizedQuestions)) {
        context.addIssue({
          code: 'custom',
          message: issue.message,
          path: issue.path,
        })
      }
    }),
)

const normalizedQuestionOptionSchema = lazySchema(() =>
  z.strictObject({
    id: identifierSchema(),
    label: z.string(),
    description: z.string(),
    recommended: z.boolean().default(false),
    preview: z
      .string()
      .max(MAX_PREVIEW_CHARS)
      .refine(value => utf8ByteLength(value) <= MAX_PREVIEW_BYTES, {
        message: `Preview must be at most ${MAX_PREVIEW_BYTES} UTF-8 bytes`,
      })
      .optional(),
  }),
)

const normalizedQuestionSchema = lazySchema(() =>
  z.strictObject({
    id: identifierSchema(),
    question: z.string(),
    header: z.string(),
    options: z.array(normalizedQuestionOptionSchema()).min(2).max(4),
    multiSelect: z.boolean(),
    minSelections: z.number().int().min(0).max(5).optional(),
    maxSelections: z.number().int().min(0).max(5).optional(),
  }),
)

const outputSchema = lazySchema(() =>
  z.strictObject({
    questions: z.array(normalizedQuestionSchema()).min(1).max(4),
    status: terminalStatusSchema(),
    response: structuredResponseSchema(),
    answers: legacyAnswersSchema(),
    answer_ids: answerIdsSchema(),
    annotations: annotationsSchema(),
  }),
)

type ModelInputSchema = ReturnType<typeof modelInputSchema>
type QuestionRequest = z.infer<ReturnType<typeof questionSchema>>
export type NormalizedQuestion = z.infer<
  ReturnType<typeof normalizedQuestionSchema>
>
type TrustedUpdatedInput = z.infer<ReturnType<typeof trustedUpdatedInputSchema>>
type StructuredResponse = z.infer<ReturnType<typeof structuredResponseSchema>>
type StructuredAnswer = z.infer<ReturnType<typeof structuredAnswerSchema>>
type OutputSchema = ReturnType<typeof outputSchema>

type ContractIssue = {
  message: string
  path: PropertyKey[]
}

// SDK request/output schemas remain public for compatibility. The trusted
// updated-input schema is exported separately so hosts and tests can validate
// UI responses without widening the model-visible request schema.
export const _sdkInputSchema = modelInputSchema
export const _trustedUpdatedInputSchema = trustedUpdatedInputSchema
export const _sdkOutputSchema = outputSchema
export type Question = QuestionRequest
export type QuestionOption = QuestionRequest['options'][number]
export type AskUserQuestionResponse = StructuredResponse
export type QuestionTerminalStatus = z.infer<ReturnType<typeof terminalStatusSchema>>
export type Output = z.infer<OutputSchema>

export const AskUserQuestionTool: Tool<ModelInputSchema, Output> = buildTool({
  name: ASK_USER_QUESTION_TOOL_NAME,
  searchHint: 'prompt the user with a multiple-choice question',
  maxResultSizeChars: 100_000,
  shouldDefer: true,
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    const format = getQuestionPreviewFormat()
    if (format === undefined) {
      return ASK_USER_QUESTION_TOOL_PROMPT
    }
    return ASK_USER_QUESTION_TOOL_PROMPT + PREVIEW_FEATURE_PROMPT[format]
  },
  get inputSchema(): ModelInputSchema {
    return modelInputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  get permissionUpdatedInputSchema() {
    return trustedUpdatedInputSchema()
  },
  userFacingName() {
    return ''
  },
  isEnabled() {
    // When --channels is active the user is likely on Telegram/Discord, not
    // watching the TUI. The multiple-choice dialog would hang with nobody at
    // the keyboard. Channel permission relay already skips
    // requiresUserInteraction() tools (interactiveHandler.ts) so there's
    // no alternate approval path.
    if (
      (feature('KAIROS') || feature('KAIROS_CHANNELS')) &&
      getAllowedChannels().length > 0
    ) {
      return false
    }
    return true
  },
  // A blocking interaction is one-at-a-time. Marking this concurrency-safe
  // allowed several question dialogs to race and made answer/request
  // correlation depend on host-specific queue behavior.
  isConcurrencySafe() {
    return false
  },
  isReadOnly() {
    return true
  },
  backfillObservableInput(input) {
    const parsed = modelInputSchema().safeParse(input)
    if (!parsed.success) return
    input.questions = normalizeQuestions(parsed.data.questions)
  },
  toAutoClassifierInput(input) {
    return input.questions.map(question => question.question).join(' | ')
  },
  requiresUserInteraction() {
    return true
  },
  async validateInput({ questions }) {
    if (getQuestionPreviewFormat() !== 'html') {
      return { result: true }
    }
    for (const question of questions) {
      for (const option of question.options) {
        const error = validateHtmlPreview(option.preview)
        if (error) {
          return {
            result: false,
            message: `Option "${option.label}" in question "${question.question}": ${error}`,
            errorCode: 1,
          }
        }
      }
    }
    return { result: true }
  },
  async checkPermissions(input) {
    return {
      behavior: 'ask' as const,
      message: 'Answer questions?',
      updatedInput: {
        ...input,
        questions: normalizeQuestions(input.questions),
      },
    }
  },
  ...createToolPresentationDelegates(ASK_USER_QUESTION_TOOL_NAME, [
    'renderToolUseMessage',
    'renderToolUseProgressMessage',
    'renderToolResultMessage',
    'renderToolUseRejectedMessage',
    'renderToolUseErrorMessage',
  ]),
  async call(input, _context) {
    const parsed = trustedUpdatedInputSchema().safeParse(input)
    if (!parsed.success) {
      const problems = parsed.error.issues
        .slice(0, 5)
        .map(issue => `${issue.path.join('.') || '<root>'}: ${issue.message}`)
        .join('; ')
      throw new Error(
        `AskUserQuestion requires a validated response from the user interaction host; ${problems}`,
      )
    }

    const questions = normalizeQuestions(parsed.data.questions)
    const response = canonicalResponse(parsed.data, questions)
    const compatibility = compatibilityFromResponse(response, questions)

    return {
      data: {
        questions,
        status: response.status,
        response,
        answers:
          response.status === 'submitted'
            ? parsed.data.answers ?? compatibility.answers
            : {},
        answer_ids:
          response.status === 'submitted'
            ? parsed.data.answer_ids ?? compatibility.answer_ids
            : {},
        ...(response.status === 'submitted' &&
          (parsed.data.annotations ?? compatibility.annotations) && {
            annotations: parsed.data.annotations ?? compatibility.annotations,
          }),
      },
    }
  },
  mapToolResultToToolResultBlockParam(result, toolUseID) {
    if (result.status !== 'submitted') {
      const reason = result.response.reason
        ? ` Reason: ${JSON.stringify(result.response.reason)}.`
        : ''
      return {
        type: 'tool_result',
        content: `The user-question interaction ended with status "${result.status}".${reason} No answers were submitted; do not infer or fabricate user choices.`,
        tool_use_id: toolUseID,
      }
    }

    const questionsById = new Map(
      result.questions.map(question => [question.id, question]),
    )
    const structuredForModel = result.response.answers.map(answer => {
      const question = questionsById.get(answer.questionId)
      const optionsById = new Map(
        question?.options.map(option => [option.id, option.label]) ?? [],
      )
      return {
        questionId: answer.questionId,
        question: question?.question,
        selectedOptions: answer.selectedOptionIds.map(optionId => ({
          id: optionId,
          label: optionsById.get(optionId),
        })),
        ...(answer.customText !== undefined && {
          customText: answer.customText,
        }),
        ...(answer.notes !== undefined && { notes: answer.notes }),
      }
    })
    return {
      type: 'tool_result',
      content: `User submitted the following structured answers:\n${JSON.stringify(structuredForModel, null, 2)}`,
      tool_use_id: toolUseID,
    }
  },
} satisfies ToolDef<ModelInputSchema, Output>)

function addQuestionDefinitionIssues(
  data: { questions: QuestionRequest[] },
  context: z.RefinementCtx,
): void {
  const questionTexts = new Map<string, number>()
  const questionIds = new Map<string, number>()

  data.questions.forEach((question, questionIndex) => {
    const normalizedText = normalizedIdentityText(question.question)
    const previousQuestion = questionTexts.get(normalizedText)
    if (previousQuestion !== undefined) {
      context.addIssue({
        code: 'custom',
        message: `Question text duplicates question ${previousQuestion + 1}`,
        path: ['questions', questionIndex, 'question'],
      })
    } else {
      questionTexts.set(normalizedText, questionIndex)
    }

    if (question.id) {
      const previousId = questionIds.get(question.id)
      if (previousId !== undefined) {
        context.addIssue({
          code: 'custom',
          message: `Question ID duplicates question ${previousId + 1}`,
          path: ['questions', questionIndex, 'id'],
        })
      } else {
        questionIds.set(question.id, questionIndex)
      }
    }

    const labels = new Map<string, number>()
    const optionIds = new Map<string, number>()
    question.options.forEach((option, optionIndex) => {
      const normalizedLabel = normalizedIdentityText(option.label)
      const previousLabel = labels.get(normalizedLabel)
      if (previousLabel !== undefined) {
        context.addIssue({
          code: 'custom',
          message: `Option label duplicates option ${previousLabel + 1}`,
          path: ['questions', questionIndex, 'options', optionIndex, 'label'],
        })
      } else {
        labels.set(normalizedLabel, optionIndex)
      }

      if (option.id) {
        const previousId = optionIds.get(option.id)
        if (previousId !== undefined) {
          context.addIssue({
            code: 'custom',
            message: `Option ID duplicates option ${previousId + 1}`,
            path: ['questions', questionIndex, 'options', optionIndex, 'id'],
          })
        } else {
          optionIds.set(option.id, optionIndex)
        }
      }

      if (question.multiSelect && option.preview !== undefined) {
        context.addIssue({
          code: 'custom',
          message: 'Option previews are supported only for single-select questions',
          path: ['questions', questionIndex, 'options', optionIndex, 'preview'],
        })
      }
    })

    if (!question.multiSelect) {
      if (question.minSelections !== undefined) {
        context.addIssue({
          code: 'custom',
          message: 'minSelections is supported only for multi-select questions',
          path: ['questions', questionIndex, 'minSelections'],
        })
      }
      if (question.maxSelections !== undefined) {
        context.addIssue({
          code: 'custom',
          message: 'maxSelections is supported only for multi-select questions',
          path: ['questions', questionIndex, 'maxSelections'],
        })
      }
      return
    }

    const selectionCapacity = question.options.length + 1
    const minSelections = question.minSelections ?? 1
    const maxSelections = question.maxSelections ?? selectionCapacity
    if (minSelections > maxSelections) {
      context.addIssue({
        code: 'custom',
        message: 'minSelections must be less than or equal to maxSelections',
        path: ['questions', questionIndex, 'minSelections'],
      })
    }
    if (maxSelections > selectionCapacity) {
      context.addIssue({
        code: 'custom',
        message: `maxSelections must not exceed ${selectionCapacity} (the option count plus Other)`,
        path: ['questions', questionIndex, 'maxSelections'],
      })
    }
  })
}

function normalizedIdentityText(value: string): string {
  return Array.from(value.trim().normalize('NFKC'), codePoint =>
    codePoint.toLowerCase(),
  ).join('')
}

function utf8ByteLength(value: string): number {
  return UTF8_ENCODER.encode(value).byteLength
}

function normalizeQuestions(questions: QuestionRequest[]): NormalizedQuestion[] {
  const explicitQuestionIds = new Set(
    questions.flatMap(question => (question.id ? [question.id] : [])),
  )
  const usedQuestionIds = new Set<string>()

  return questions.map((question, questionIndex) => {
    const questionId =
      question.id ??
      allocateStableId(
        'question',
        `-${questionIndex + 1}`,
        explicitQuestionIds,
        usedQuestionIds,
      )
    usedQuestionIds.add(questionId)

    const explicitOptionIds = new Set(
      question.options.flatMap(option => (option.id ? [option.id] : [])),
    )
    const usedOptionIds = new Set<string>()
    const options = question.options.map((option, optionIndex) => {
      const optionId =
        option.id ??
        allocateStableId(
          questionId,
          `-option-${optionIndex + 1}`,
          explicitOptionIds,
          usedOptionIds,
        )
      usedOptionIds.add(optionId)
      return {
        ...option,
        id: optionId,
        recommended: option.recommended ?? false,
      }
    })

    return {
      ...question,
      id: questionId,
      options,
      ...(question.multiSelect && {
        minSelections: question.minSelections ?? 1,
        maxSelections: question.maxSelections ?? question.options.length + 1,
      }),
    }
  })
}

function allocateStableId(
  prefix: string,
  semanticSuffix: string,
  reserved: Set<string>,
  used: Set<string>,
): string {
  let collision = 1
  for (;;) {
    const collisionSuffix = collision === 1 ? '' : `-${collision}`
    const suffix = `${semanticSuffix}${collisionSuffix}`
    const prefixLimit = MAX_IDENTIFIER_CHARS - suffix.length
    const candidate = `${prefix.slice(0, prefixLimit)}${suffix}`
    if (!reserved.has(candidate) && !used.has(candidate)) {
      return candidate
    }
    collision += 1
  }
}

function trustedResponseIssues(
  input: TrustedUpdatedInput,
  questions: NormalizedQuestion[],
): ContractIssue[] {
  const issues: ContractIssue[] = []
  const hasCompatibilityPayload =
    input.answers !== undefined || input.answer_ids !== undefined

  if (
    input.response === undefined &&
    input.status === undefined &&
    !hasCompatibilityPayload
  ) {
    issues.push({
      message:
        'A trusted interaction response must include response, status, answers, or answer_ids',
      path: [],
    })
    return issues
  }

  if (
    input.response !== undefined &&
    input.status !== undefined &&
    input.response.status !== input.status
  ) {
    issues.push({
      message: 'Top-level status must match response.status',
      path: ['status'],
    })
  }
  if (
    input.response?.reason !== undefined &&
    input.reason !== undefined &&
    input.response.reason !== input.reason
  ) {
    issues.push({
      message: 'Top-level reason must match response.reason',
      path: ['reason'],
    })
  }

  const status = input.response?.status ?? input.status ?? 'submitted'
  if (status !== 'submitted') {
    if ((input.response?.answers.length ?? 0) > 0) {
      issues.push({
        message: `${status} responses must not contain answers`,
        path: ['response', 'answers'],
      })
    }
    if (input.answers && Object.keys(input.answers).length > 0) {
      issues.push({
        message: `${status} responses must not contain legacy answers`,
        path: ['answers'],
      })
    }
    if (input.answer_ids && Object.keys(input.answer_ids).length > 0) {
      issues.push({
        message: `${status} responses must not contain answer_ids`,
        path: ['answer_ids'],
      })
    }
    if (input.annotations && Object.keys(input.annotations).length > 0) {
      issues.push({
        message: `${status} responses must not contain annotations`,
        path: ['annotations'],
      })
    }
    return issues
  }

  validateCompatibilityKeys(input, questions, issues)
  const response = canonicalResponseUnchecked(input, questions)
  validateSubmittedResponse(response, questions, issues)

  if (input.response || (input.answers && input.answer_ids)) {
    const compatibility = compatibilityFromResponse(response, questions)
    if (
      input.answer_ids &&
      !answerIdRecordsEqual(input.answer_ids, compatibility.answer_ids)
    ) {
      issues.push({
        message: 'answer_ids must agree with response.answers',
        path: ['answer_ids'],
      })
    }
    if (input.answers && !stringRecordsEqual(input.answers, compatibility.answers)) {
      issues.push({
        message: 'Legacy answers must agree with response.answers',
        path: ['answers'],
      })
    }
  }

  validateAnnotations(input.annotations, response, questions, issues)
  return issues
}

function validateCompatibilityKeys(
  input: TrustedUpdatedInput,
  questions: NormalizedQuestion[],
  issues: ContractIssue[],
): void {
  if (input.answers) {
    const expected = new Set(questions.map(question => question.question))
    for (const key of Object.keys(input.answers)) {
      if (!expected.has(key)) {
        issues.push({
          message: `Legacy answer references unknown question text ${JSON.stringify(key)}`,
          path: ['answers', key],
        })
      }
    }
  }
  if (input.answer_ids) {
    const expected = new Set(questions.map(question => question.id))
    for (const key of Object.keys(input.answer_ids)) {
      if (!expected.has(key)) {
        issues.push({
          message: `answer_ids references unknown question ID ${JSON.stringify(key)}`,
          path: ['answer_ids', key],
        })
      }
    }
  }
  if (input.annotations) {
    const expected = new Set(questions.map(question => question.question))
    for (const key of Object.keys(input.annotations)) {
      if (!expected.has(key)) {
        issues.push({
          message: `Annotation references unknown question text ${JSON.stringify(key)}`,
          path: ['annotations', key],
        })
      }
    }
  }
}

function validateSubmittedResponse(
  response: StructuredResponse,
  questions: NormalizedQuestion[],
  issues: ContractIssue[],
): void {
  if (response.status !== 'submitted') return

  const questionsById = new Map(
    questions.map(question => [question.id, question]),
  )
  const seenQuestionIds = new Set<string>()
  response.answers.forEach((answer, answerIndex) => {
    const question = questionsById.get(answer.questionId)
    if (!question) {
      issues.push({
        message: `Answer references unknown question ID ${JSON.stringify(answer.questionId)}`,
        path: ['response', 'answers', answerIndex, 'questionId'],
      })
      return
    }
    if (!seenQuestionIds.add(answer.questionId)) {
      issues.push({
        message: `Question ${JSON.stringify(answer.questionId)} was answered more than once`,
        path: ['response', 'answers', answerIndex, 'questionId'],
      })
    }
    if (
      answer.customText !== undefined &&
      utf8ByteLength(answer.customText) > MAX_CUSTOM_TEXT_BYTES
    ) {
      issues.push({
        message: `Custom Other text must be at most ${MAX_CUSTOM_TEXT_BYTES} UTF-8 bytes`,
        path: ['response', 'answers', answerIndex, 'customText'],
      })
    }
    if (
      answer.notes !== undefined &&
      utf8ByteLength(answer.notes) > MAX_NOTES_BYTES
    ) {
      issues.push({
        message: `Notes must be at most ${MAX_NOTES_BYTES} UTF-8 bytes`,
        path: ['response', 'answers', answerIndex, 'notes'],
      })
    }

    const optionIds = new Set(question.options.map(option => option.id))
    const seenOptionIds = new Set<string>()
    answer.selectedOptionIds.forEach((optionId, optionIndex) => {
      if (!optionIds.has(optionId)) {
        issues.push({
          message: `Selected option ID ${JSON.stringify(optionId)} does not belong to question ${JSON.stringify(question.id)}`,
          path: [
            'response',
            'answers',
            answerIndex,
            'selectedOptionIds',
            optionIndex,
          ],
        })
      }
      if (!seenOptionIds.add(optionId)) {
        issues.push({
          message: `Selected option ID ${JSON.stringify(optionId)} appears more than once`,
          path: [
            'response',
            'answers',
            answerIndex,
            'selectedOptionIds',
            optionIndex,
          ],
        })
      }
    })

    if (!question.multiSelect && answer.selectedOptionIds.length > 1) {
      issues.push({
        message: 'Single-select questions accept at most one selectedOptionId',
        path: ['response', 'answers', answerIndex, 'selectedOptionIds'],
      })
    }
    if (
      !question.multiSelect &&
      answer.selectedOptionIds.length > 0 &&
      answer.customText !== undefined
    ) {
      issues.push({
        message:
          'A single-select answer must choose either one option or custom Other text, not both',
        path: ['response', 'answers', answerIndex],
      })
    }
    const selectionCount =
      answer.selectedOptionIds.length +
      (answer.customText === undefined ? 0 : 1)
    if (question.multiSelect) {
      const minSelections = question.minSelections ?? 1
      const maxSelections =
        question.maxSelections ?? question.options.length + 1
      if (selectionCount < minSelections) {
        issues.push({
          message: `Submitted answer requires at least ${minSelections} selection${minSelections === 1 ? '' : 's'} (Other counts as one)`,
          path: ['response', 'answers', answerIndex],
        })
      }
      if (selectionCount > maxSelections) {
        issues.push({
          message: `Submitted answer allows at most ${maxSelections} selection${maxSelections === 1 ? '' : 's'} (Other counts as one)`,
          path: ['response', 'answers', answerIndex],
        })
      }
    } else if (selectionCount === 0) {
      issues.push({
        message: 'Submitted answers must select an option or provide custom Other text',
        path: ['response', 'answers', answerIndex],
      })
    }
  })

  for (const question of questions) {
    if (!seenQuestionIds.has(question.id)) {
      issues.push({
        message: `Submitted response is missing question ${JSON.stringify(question.id)}`,
        path: ['response', 'answers'],
      })
    }
  }
}

function validateAnnotations(
  annotations: TrustedUpdatedInput['annotations'],
  response: StructuredResponse,
  questions: NormalizedQuestion[],
  issues: ContractIssue[],
): void {
  if (!annotations || response.status !== 'submitted') return
  const answersById = new Map(
    response.answers.map(answer => [answer.questionId, answer]),
  )
  for (const question of questions) {
    const annotation = annotations[question.question]
    const preview = annotation?.preview
    const answer = answersById.get(question.id)
    if (
      annotation?.notes !== undefined &&
      annotation.notes !== answer?.notes
    ) {
      issues.push({
        message: 'Annotation notes must agree with the structured answer notes',
        path: ['annotations', question.question, 'notes'],
      })
    }
    if (preview === undefined) continue
    const selectedPreviews = new Set(
      question.options
        .filter(option => answer?.selectedOptionIds.includes(option.id))
        .flatMap(option => (option.preview === undefined ? [] : [option.preview])),
    )
    if (!selectedPreviews.has(preview)) {
      issues.push({
        message: 'Annotation preview must belong to a selected option',
        path: ['annotations', question.question, 'preview'],
      })
    }
  }
}

function canonicalResponse(
  input: TrustedUpdatedInput,
  questions: NormalizedQuestion[],
): StructuredResponse {
  return canonicalResponseUnchecked(input, questions)
}

function canonicalResponseUnchecked(
  input: TrustedUpdatedInput,
  questions: NormalizedQuestion[],
): StructuredResponse {
  if (input.response) {
    const questionsById = new Map(
      questions.map(question => [question.id, question]),
    )
    return {
      ...input.response,
      answers: input.response.answers.map(answer => {
        const question = questionsById.get(answer.questionId)
        const legacyNotes = question
          ? input.annotations?.[question.question]?.notes
          : undefined
        return {
          ...answer,
          ...(answer.notes === undefined &&
            legacyNotes !== undefined && { notes: legacyNotes }),
        }
      }),
      ...(input.response.reason === undefined &&
        input.reason !== undefined && { reason: input.reason }),
    }
  }
  const status = input.status ?? 'submitted'
  if (status !== 'submitted') {
    return {
      status,
      answers: [],
      ...(input.reason !== undefined && { reason: input.reason }),
    }
  }

  return {
    status: 'submitted',
    answers: questions.map(question =>
      compatibilityAnswerForQuestion(input, question),
    ),
    ...(input.reason !== undefined && { reason: input.reason }),
  }
}

function compatibilityAnswerForQuestion(
  input: TrustedUpdatedInput,
  question: NormalizedQuestion,
): StructuredAnswer {
  const legacyAnswer = input.answers?.[question.question]?.trim()
  const explicitOptionIds = input.answer_ids?.[question.id]
  const selectedOptionIds =
    explicitOptionIds ?? parseLegacySelectedOptionIds(question, legacyAnswer)
  const selectedLabels = selectedOptionIds.flatMap(optionId => {
    const option = question.options.find(candidate => candidate.id === optionId)
    return option ? [option.label] : []
  })
  const selectedText = selectedLabels.join(', ')
  let customText: string | undefined
  if (legacyAnswer) {
    if (selectedOptionIds.length === 0) {
      customText = legacyAnswer
    } else if (legacyAnswer !== selectedText) {
      const prefix = `${selectedText}, `
      customText = legacyAnswer.startsWith(prefix)
        ? legacyAnswer.slice(prefix.length).trim() || undefined
        : legacyAnswer
    }
  }
  const notes = input.annotations?.[question.question]?.notes
  return {
    questionId: question.id,
    selectedOptionIds,
    ...(customText !== undefined && { customText }),
    ...(notes !== undefined && { notes }),
  }
}

function parseLegacySelectedOptionIds(
  question: NormalizedQuestion,
  answer: string | undefined,
): string[] {
  if (!answer) return []
  const exact = question.options.find(
    option => normalizedIdentityText(option.label) === normalizedIdentityText(answer),
  )
  if (exact) return [exact.id]
  if (!question.multiSelect) return []

  const labelsByIdentity = new Map(
    question.options.map(option => [normalizedIdentityText(option.label), option.id]),
  )
  const pieces = answer.split(',').map(piece => normalizedIdentityText(piece))
  const selected = pieces.map(piece => labelsByIdentity.get(piece))
  if (selected.some(optionId => optionId === undefined)) return []
  return [...new Set(selected as string[])]
}

function compatibilityFromResponse(
  response: StructuredResponse,
  questions: NormalizedQuestion[],
): {
  answers: Record<string, string>
  answer_ids: Record<string, string[]>
  annotations?: Record<string, { preview?: string; notes?: string }>
} {
  if (response.status !== 'submitted') {
    return { answers: {}, answer_ids: {} }
  }

  const answersById = new Map(
    response.answers.map(answer => [answer.questionId, answer]),
  )
  const answers: Record<string, string> = {}
  const answer_ids: Record<string, string[]> = {}
  const annotations: Record<
    string,
    { preview?: string; notes?: string }
  > = {}

  for (const question of questions) {
    const answer = answersById.get(question.id)
    if (!answer) continue
    const selectedOptions = answer.selectedOptionIds.flatMap(optionId => {
      const option = question.options.find(candidate => candidate.id === optionId)
      return option ? [option] : []
    })
    answer_ids[question.id] = answer.selectedOptionIds
    answers[question.question] = [
      ...selectedOptions.map(option => option.label),
      ...(answer.customText === undefined ? [] : [answer.customText]),
    ].join(', ')

    const preview =
      selectedOptions.length === 1 ? selectedOptions[0]?.preview : undefined
    if (preview !== undefined || answer.notes !== undefined) {
      annotations[question.question] = {
        ...(preview !== undefined && { preview }),
        ...(answer.notes !== undefined && { notes: answer.notes }),
      }
    }
  }
  return {
    answers,
    answer_ids,
    ...(Object.keys(annotations).length > 0 && { annotations }),
  }
}

function stringRecordsEqual(
  left: Record<string, string>,
  right: Record<string, string>,
): boolean {
  const leftKeys = Object.keys(left).sort()
  const rightKeys = Object.keys(right).sort()
  return (
    leftKeys.length === rightKeys.length &&
    leftKeys.every(
      (key, index) =>
        key === rightKeys[index] && left[key]?.trim() === right[key]?.trim(),
    )
  )
}

function answerIdRecordsEqual(
  left: Record<string, string[]>,
  right: Record<string, string[]>,
): boolean {
  const leftKeys = Object.keys(left).sort()
  const rightKeys = Object.keys(right).sort()
  return (
    leftKeys.length === rightKeys.length &&
    leftKeys.every((key, index) => {
      if (key !== rightKeys[index]) return false
      const leftIds = [...(left[key] ?? [])].sort()
      const rightIds = [...(right[key] ?? [])].sort()
      return (
        leftIds.length === rightIds.length &&
        leftIds.every((optionId, optionIndex) => optionId === rightIds[optionIndex])
      )
    })
  )
}

// Lightweight HTML fragment check. Not a parser — HTML5 parsers are
// error-recovering by spec and accept anything. We're checking model intent
// (did it emit HTML?) and catching the specific things we told it not to do.
function validateHtmlPreview(preview: string | undefined): string | null {
  if (preview === undefined) return null
  if (utf8ByteLength(preview) > MAX_PREVIEW_BYTES) {
    return `preview must be at most ${MAX_PREVIEW_BYTES} UTF-8 bytes`
  }
  if (/<\s*(html|body|!doctype)\b/i.test(preview)) {
    return 'preview must be an HTML fragment, not a full document (no <html>, <body>, or <!DOCTYPE>)'
  }
  // SDK consumers commonly render this fragment through innerHTML. Keep the
  // accepted language deliberately smaller than HTML: no active/embedded
  // namespaces, document metadata, forms, or elements that fetch resources.
  if (
    /<\s*\/?\s*(script|style|iframe|object|embed|form|input|button|textarea|select|option|meta|link|base|svg|math|img|picture|video|audio|source|track|a|area|details|summary|dialog|portal|fencedframe)\b/i.test(
      preview,
    )
  ) {
    return 'preview contains a forbidden active, embedded, form, metadata, or resource-loading element'
  }

  for (const attribute of htmlAttributes(preview)) {
    if (/^on[a-z0-9_:-]+$/i.test(attribute.name)) {
      return `preview must not contain event-handler attributes such as ${attribute.name}`
    }
    if (attribute.name === 'srcdoc') {
      return 'preview must not contain srcdoc attributes'
    }

    const decodedValue = decodeHtmlCharacterReferences(attribute.value).trim()
    if (
      ['src', 'srcset', 'poster', 'background'].includes(attribute.name) &&
      decodedValue.length > 0
    ) {
      return `preview must not load external or embedded resources via ${attribute.name}`
    }
    if (
      ['href', 'xlink:href', 'action', 'formaction'].includes(attribute.name) &&
      decodedValue.length > 0 &&
      !decodedValue.startsWith('#')
    ) {
      const compactValue = decodedValue
        .replace(/[\u0000-\u0020\u007f-\u009f]/g, '')
        .toLocaleLowerCase('en-US')
      if (/^(javascript|vbscript|data):/.test(compactValue)) {
        return `preview contains a dangerous URL in ${attribute.name}`
      }
      return `preview must not navigate to or submit to a non-fragment URL via ${attribute.name}`
    }

    if (attribute.name === 'style') {
      const css = decodeHtmlCharacterReferences(attribute.value)
        .replace(/\/\*[\s\S]*?\*\//g, '')
        .toLocaleLowerCase('en-US')
      if (
        css.includes('\\') ||
        /(?:url\s*\(|(?:-webkit-)?image-set\s*\(|@import\b|expression\s*\(|behavior\s*:|-moz-binding\s*:)/.test(
          css,
        )
      ) {
        return 'preview inline styles must not contain URLs, imports, expressions, behaviors, or CSS escapes'
      }
    }
  }
  if (!/<[a-z][^>]*>/i.test(preview)) {
    return 'preview must contain HTML (previewFormat is set to "html"). Wrap content in a tag like <div> or <pre>.'
  }
  return null
}

function htmlAttributes(
  fragment: string,
): Array<{ name: string; value: string }> {
  const attributes: Array<{ name: string; value: string }> = []
  // HTML's tokenizer recovers `<tag/attribute=value>` as an attribute even
  // though the slash normally introduces a self-closing tag. Treat `/` as an
  // attribute boundary too so malformed-but-loadable markup cannot bypass
  // the event and resource checks above.
  const attributePattern =
    /(?:^|[\s</])([^\s"'<>/=]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>]+))/g
  for (const match of fragment.matchAll(attributePattern)) {
    attributes.push({
      name: (match[1] ?? '').toLocaleLowerCase('en-US'),
      value: match[2] ?? match[3] ?? match[4] ?? '',
    })
  }
  return attributes
}

function decodeHtmlCharacterReferences(value: string): string {
  const named: Record<string, string> = {
    amp: '&',
    apos: "'",
    colon: ':',
    gt: '>',
    lt: '<',
    newline: '\n',
    quot: '"',
    tab: '\t',
  }
  return value.replace(
    /&(?:#(\d+)|#x([0-9a-f]+)|([a-z]+));?/gi,
    (reference, decimal: string, hexadecimal: string, name: string) => {
      const codePoint = decimal
        ? Number.parseInt(decimal, 10)
        : hexadecimal
          ? Number.parseInt(hexadecimal, 16)
          : undefined
      if (codePoint !== undefined) {
        if (
          !Number.isInteger(codePoint) ||
          codePoint <= 0 ||
          codePoint > 0x10ffff ||
          (codePoint >= 0xd800 && codePoint <= 0xdfff)
        ) {
          return '\uFFFD'
        }
        return String.fromCodePoint(codePoint)
      }
      return named[name?.toLocaleLowerCase('en-US')] ?? reference
    },
  )
}
