import { afterEach, describe, expect, test } from 'bun:test'

import { setQuestionPreviewFormat } from '../../src/bootstrap/state.js'
import {
  AskUserQuestionTool,
  _sdkInputSchema,
  _sdkOutputSchema,
  _trustedUpdatedInputSchema,
  type Output,
} from '../../src/tools/AskUserQuestionTool/AskUserQuestionTool.js'
import { parseToolUpdatedInput } from '../../src/services/tools/toolExecution.js'

const questionWithoutIds = {
  question: 'Which color?',
  header: 'Color',
  options: [
    { label: 'Red', description: 'Use red.' },
    { label: 'Blue', description: 'Use blue.' },
  ],
  multiSelect: false,
}

const normalizedQuestion = {
  id: 'color',
  question: 'Which color?',
  header: 'Color',
  options: [
    { id: 'red', label: 'Red', description: 'Use red.' },
    { id: 'blue', label: 'Blue', description: 'Use blue.' },
  ],
  multiSelect: false,
}

async function invokeTrusted(input: unknown): Promise<Output> {
  const result = await AskUserQuestionTool.call(input as never, {} as never)
  return result.data
}

afterEach(() => {
  setQuestionPreviewFormat(undefined)
})

describe('AskUserQuestion model request boundary', () => {
  test('keeps trusted response fields out of the model-visible schema', () => {
    expect(
      _sdkInputSchema().safeParse({ questions: [questionWithoutIds] }).success,
    ).toBe(true)

    for (const injected of [
      { status: 'submitted' },
      { response: { status: 'declined', answers: [] } },
      { answers: { 'Which color?': 'Red' } },
      { answer_ids: { color: ['red'] } },
      { annotations: {} },
    ]) {
      expect(
        _sdkInputSchema().safeParse({
          questions: [questionWithoutIds],
          ...injected,
        }).success,
      ).toBe(false)
    }
  })

  test('accepts trusted responses only from the permission host, never a hook', () => {
    const trustedResponse = {
      questions: [normalizedQuestion],
      response: {
        status: 'submitted' as const,
        answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
      },
    }
    expect(AskUserQuestionTool.permissionUpdatedInputSchema).toBe(
      _trustedUpdatedInputSchema(),
    )
    expect(
      parseToolUpdatedInput(
        AskUserQuestionTool,
        trustedResponse,
        'permissionHost',
      ).success,
    ).toBe(true)
    expect(
      parseToolUpdatedInput(AskUserQuestionTool, trustedResponse, 'hook')
        .success,
    ).toBe(false)
  })

  test('rejects normalized duplicate text and option labels', () => {
    const duplicateQuestion = {
      ...questionWithoutIds,
      question: '  WHICH COLOR? ',
    }
    const duplicateOption = {
      ...questionWithoutIds,
      options: [
        { label: 'Red', description: 'First.' },
        { label: ' red ', description: 'Second.' },
      ],
    }

    expect(
      _sdkInputSchema().safeParse({
        questions: [questionWithoutIds, duplicateQuestion],
      }).success,
    ).toBe(false)
    expect(
      _sdkInputSchema().safeParse({ questions: [duplicateOption] }).success,
    ).toBe(false)
  })

  test('normalizes identity per Unicode code point across compatibility and whitespace edges', () => {
    const duplicateLabelPairs = [
      ['ΟΣ', 'οσ'],
      ['İ', `i\u0307`],
      ['\u{10400}', '\u{10428}'],
      ['\uFEFF\u00A0Red\u2029', 'red'],
    ]

    for (const [first, second] of duplicateLabelPairs) {
      expect(
        _sdkInputSchema().safeParse({
          questions: [
            {
              ...questionWithoutIds,
              options: [
                { label: first, description: 'First.' },
                { label: second, description: 'Second.' },
              ],
            },
          ],
        }).success,
      ).toBe(false)
    }
  })

  test('keeps recommendation metadata separate from labels and never selects it', async () => {
    const parsed = _sdkInputSchema().parse({
      questions: [
        {
          ...questionWithoutIds,
          options: [
            { ...questionWithoutIds.options[0], recommended: true },
            questionWithoutIds.options[1],
          ],
        },
      ],
    })
    expect(parsed.questions[0]!.options).toMatchObject([
      { label: 'Red', recommended: true },
      { label: 'Blue', recommended: false },
    ])
    expect(parsed.questions[0]!.options[0]!.label).not.toContain('Recommended')
    expect(await AskUserQuestionTool.prompt()).toContain('must never preselect')
  })

  test('uses the same UTF-8 byte limit for markdown and HTML previews', () => {
    const oversizedUnicodePreview = '界'.repeat(33_334)
    expect(
      _sdkInputSchema().safeParse({
        questions: [
          {
            ...questionWithoutIds,
            options: [
              {
                ...questionWithoutIds.options[0],
                preview: oversizedUnicodePreview,
              },
              questionWithoutIds.options[1],
            ],
          },
        ],
      }).success,
    ).toBe(false)
  })

  test('backfills deterministic, collision-free question and option IDs', async () => {
    const request = {
      questions: [
        questionWithoutIds,
        {
          ...questionWithoutIds,
          id: 'question-2',
          question: 'Which theme?',
          header: 'Theme',
          options: [
            {
              id: 'question-2-option-1',
              label: 'Light',
              description: 'Use light mode.',
            },
            { label: 'Dark', description: 'Use dark mode.' },
          ],
        },
      ],
    }

    const first = await AskUserQuestionTool.checkPermissions(request)
    const second = await AskUserQuestionTool.checkPermissions(request)
    expect(first.updatedInput).toEqual(second.updatedInput)
    expect(first.updatedInput.questions).toEqual([
      {
        ...questionWithoutIds,
        id: 'question-1',
        options: [
          {
            ...questionWithoutIds.options[0],
            id: 'question-1-option-1',
            recommended: false,
          },
          {
            ...questionWithoutIds.options[1],
            id: 'question-1-option-2',
            recommended: false,
          },
        ],
      },
      {
        ...request.questions[1],
        options: [
          { ...request.questions[1]!.options[0], recommended: false },
          {
            ...request.questions[1]!.options[1],
            id: 'question-2-option-2',
            recommended: false,
          },
        ],
      },
    ])
  })

  test('bounds generated option IDs for a maximum-length parent and collision suffix', async () => {
    const questionId = 'q'.repeat(128)
    const reservedBase = `${'q'.repeat(119)}-option-1`
    const generatedAfterCollision = `${'q'.repeat(117)}-option-1-2`
    const request = {
      questions: [
        {
          ...questionWithoutIds,
          id: questionId,
          options: [
            { label: 'Red', description: 'Use red.' },
            {
              id: reservedBase,
              label: 'Blue',
              description: 'Use blue.',
            },
          ],
        },
      ],
    }

    const permission = await AskUserQuestionTool.checkPermissions(request)
    const normalized = permission.updatedInput.questions[0]!
    expect(normalized.options[0]!.id).toBe(generatedAfterCollision)
    expect(normalized.options[0]!.id).toHaveLength(128)
    expect(normalized.options[1]!.id).toBe(reservedBase)

    const submitted = await invokeTrusted({
      ...permission.updatedInput,
      response: {
        status: 'submitted',
        answers: [
          {
            questionId,
            selectedOptionIds: [generatedAfterCollision],
          },
        ],
      },
    })
    expect(submitted.answer_ids).toEqual({
      [questionId]: [generatedAfterCollision],
    })
  })

  test('marks the blocking interaction as concurrency-unsafe', () => {
    expect(AskUserQuestionTool.isConcurrencySafe()).toBe(false)
  })

  test('normalizes and validates multi-select cardinality including Other', async () => {
    const multiSelectQuestion = {
      ...questionWithoutIds,
      question: 'Which colors?',
      multiSelect: true,
    }
    const permission = await AskUserQuestionTool.checkPermissions({
      questions: [multiSelectQuestion],
    })
    expect(permission.updatedInput.questions[0]).toMatchObject({
      minSelections: 1,
      maxSelections: 3,
    })

    expect(
      _sdkInputSchema().safeParse({
        questions: [
          { ...questionWithoutIds, minSelections: 0, maxSelections: 1 },
        ],
      }).success,
    ).toBe(false)
    expect(
      _sdkInputSchema().safeParse({
        questions: [
          { ...multiSelectQuestion, minSelections: 3, maxSelections: 2 },
        ],
      }).success,
    ).toBe(false)
    expect(
      _sdkInputSchema().safeParse({
        questions: [{ ...multiSelectQuestion, maxSelections: 4 }],
      }).success,
    ).toBe(false)
  })
})

describe('AskUserQuestion trusted response contract', () => {
  test('accepts canonical multi-select plus Other and emits compatibility records', async () => {
    const questions = [
      normalizedQuestion,
      {
        id: 'features',
        question: 'Which features?',
        header: 'Features',
        options: [
          { id: 'logs', label: 'Logs', description: 'Enable logs.' },
          { id: 'metrics', label: 'Metrics', description: 'Enable metrics.' },
        ],
        multiSelect: true,
      },
    ]
    const result = await invokeTrusted({
      questions,
      response: {
        status: 'submitted',
        answers: [
          { questionId: 'color', selectedOptionIds: ['red'] },
          {
            questionId: 'features',
            selectedOptionIds: ['logs', 'metrics'],
            customText: 'Tracing',
            notes: 'Prefer low overhead.',
          },
        ],
      },
    })

    expect(result.status).toBe('submitted')
    expect(result.answers).toEqual({
      'Which color?': 'Red',
      'Which features?': 'Logs, Metrics, Tracing',
    })
    expect(result.answer_ids).toEqual({
      color: ['red'],
      features: ['logs', 'metrics'],
    })
    expect(result.annotations).toEqual({
      'Which features?': { notes: 'Prefer low overhead.' },
    })
    expect(() => _sdkOutputSchema().parse(result)).not.toThrow()
  })

  test('accepts Rust transitional top-level status, answer_ids, and legacy answers', async () => {
    const result = await invokeTrusted({
      questions: [questionWithoutIds],
      status: 'submitted',
      answer_ids: { 'question-1': ['question-1-option-2'] },
      answers: { 'Which color?': 'Blue' },
    })

    expect(result.response).toEqual({
      status: 'submitted',
      answers: [
        {
          questionId: 'question-1',
          selectedOptionIds: ['question-1-option-2'],
        },
      ],
    })
    expect(result.answers).toEqual({ 'Which color?': 'Blue' })
  })

  test('preserves legacy question-text answers, including custom Other text', async () => {
    const optionResult = await invokeTrusted({
      questions: [questionWithoutIds],
      answers: { 'Which color?': 'Red' },
    })
    expect(optionResult.response.answers[0]).toEqual({
      questionId: 'question-1',
      selectedOptionIds: ['question-1-option-1'],
    })

    const otherResult = await invokeTrusted({
      questions: [questionWithoutIds],
      answers: { 'Which color?': 'Ultraviolet' },
    })
    expect(otherResult.response.answers[0]).toEqual({
      questionId: 'question-1',
      selectedOptionIds: [],
      customText: 'Ultraviolet',
    })
  })

  test('never treats raw requests or empty submitted records as successful answers', async () => {
    await expect(
      invokeTrusted({ questions: [questionWithoutIds] }),
    ).rejects.toThrow('validated response')
    await expect(
      invokeTrusted({
        questions: [questionWithoutIds],
        status: 'submitted',
      }),
    ).rejects.toThrow('must select an option')
    await expect(
      invokeTrusted({
        questions: [questionWithoutIds],
        answers: {},
      }),
    ).rejects.toThrow('must select an option')
  })

  test('allows an explicitly submitted empty answer only when minSelections is zero', async () => {
    const optionalQuestion = {
      ...questionWithoutIds,
      question: 'Any colors?',
      multiSelect: true,
      minSelections: 0,
      maxSelections: 3,
    }
    const result = await invokeTrusted({
      questions: [optionalQuestion],
      status: 'submitted',
    })
    expect(result.response).toEqual({
      status: 'submitted',
      answers: [
        {
          questionId: 'question-1',
          selectedOptionIds: [],
        },
      ],
    })
    expect(result.answers).toEqual({ 'Any colors?': '' })
  })

  test('counts custom Other text toward multi-select min/max cardinality', async () => {
    const boundedQuestion = {
      ...questionWithoutIds,
      id: 'colors',
      question: 'Which colors?',
      multiSelect: true,
      minSelections: 2,
      maxSelections: 2,
      options: [
        { ...questionWithoutIds.options[0], id: 'red' },
        { ...questionWithoutIds.options[1], id: 'blue' },
      ],
    }
    await expect(
      invokeTrusted({
        questions: [boundedQuestion],
        response: {
          status: 'submitted',
          answers: [{ questionId: 'colors', selectedOptionIds: ['red'] }],
        },
      }),
    ).rejects.toThrow('at least 2 selections')

    const valid = await invokeTrusted({
      questions: [boundedQuestion],
      response: {
        status: 'submitted',
        answers: [
          {
            questionId: 'colors',
            selectedOptionIds: ['red'],
            customText: 'Ultraviolet',
          },
        ],
      },
    })
    expect(valid.response.answers[0]).toMatchObject({
      selectedOptionIds: ['red'],
      customText: 'Ultraviolet',
    })

    await expect(
      invokeTrusted({
        questions: [boundedQuestion],
        response: {
          status: 'submitted',
          answers: [
            {
              questionId: 'colors',
              selectedOptionIds: ['red', 'blue'],
              customText: 'Ultraviolet',
            },
          ],
        },
      }),
    ).rejects.toThrow('at most 2 selections')
  })

  test('rejects unknown IDs, duplicate selections, missing questions, and divergent dual writes', () => {
    const invalidResponses = [
      {
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [
            { questionId: 'missing', selectedOptionIds: ['red'] },
          ],
        },
      },
      {
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [
            { questionId: 'color', selectedOptionIds: ['red', 'red'] },
          ],
        },
      },
      {
        questions: [normalizedQuestion],
        response: { status: 'submitted', answers: [] },
      },
      {
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
        },
        status: 'declined',
      },
      {
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
        },
        answer_ids: { color: ['blue'] },
      },
      {
        questions: [normalizedQuestion],
        status: 'submitted',
        answer_ids: { color: ['red'] },
        answers: { 'Which color?': 'Blue' },
      },
    ]

    for (const response of invalidResponses) {
      expect(_trustedUpdatedInputSchema().safeParse(response).success).toBe(
        false,
      )
    }
  })

  test('merges legacy notes into the canonical response and rejects divergent copies', async () => {
    const merged = await invokeTrusted({
      questions: [normalizedQuestion],
      response: {
        status: 'submitted',
        answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
      },
      annotations: { 'Which color?': { notes: 'Use a warm red.' } },
    })
    expect(merged.response.answers[0]?.notes).toBe('Use a warm red.')

    await expect(
      invokeTrusted({
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [
            {
              questionId: 'color',
              selectedOptionIds: ['red'],
              notes: 'Canonical note.',
            },
          ],
        },
        annotations: { 'Which color?': { notes: 'Different legacy note.' } },
      }),
    ).rejects.toThrow('must agree')
  })

  test('uses the same 20,000 UTF-8 byte limit as the native host for Other and notes', async () => {
    const exactlyTwentyThousandBytes = '😀'.repeat(5_000)
    const valid = await invokeTrusted({
      questions: [normalizedQuestion],
      response: {
        status: 'submitted',
        answers: [
          {
            questionId: 'color',
            selectedOptionIds: [],
            customText: exactlyTwentyThousandBytes,
          },
        ],
      },
    })
    expect(valid.response.answers[0]?.customText).toBe(
      exactlyTwentyThousandBytes,
    )

    await expect(
      invokeTrusted({
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [
            {
              questionId: 'color',
              selectedOptionIds: [],
              customText: `${exactlyTwentyThousandBytes}界`,
            },
          ],
        },
      }),
    ).rejects.toThrow('UTF-8 bytes')
    await expect(
      invokeTrusted({
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [
            {
              questionId: 'color',
              selectedOptionIds: ['red'],
              notes: `${exactlyTwentyThousandBytes}界`,
            },
          ],
        },
      }),
    ).rejects.toThrow('UTF-8 bytes')
    await expect(
      invokeTrusted({
        questions: [questionWithoutIds],
        answers: { 'Which color?': '界'.repeat(6_667) },
      }),
    ).rejects.toThrow('UTF-8 bytes')
  })

  test('enforces the UTF-8 preview limit on trusted annotations and output too', () => {
    const oversizedUnicodePreview = '界'.repeat(33_334)
    expect(
      _trustedUpdatedInputSchema().safeParse({
        questions: [normalizedQuestion],
        response: {
          status: 'submitted',
          answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
        },
        annotations: {
          'Which color?': { preview: oversizedUnicodePreview },
        },
      }).success,
    ).toBe(false)
    expect(
      _sdkOutputSchema().safeParse({
        questions: [
          {
            ...normalizedQuestion,
            options: [
              { ...normalizedQuestion.options[0], preview: oversizedUnicodePreview },
              normalizedQuestion.options[1],
            ],
          },
        ],
        status: 'submitted',
        response: {
          status: 'submitted',
          answers: [{ questionId: 'color', selectedOptionIds: ['red'] }],
        },
        answers: { 'Which color?': 'Red' },
        answer_ids: { color: ['red'] },
      }).success,
    ).toBe(false)
  })

  test('represents decline/cancel/timeout as explicit answer-free terminal states', async () => {
    for (const status of [
      'declined',
      'cancelled',
      'timed_out',
      'interaction_unavailable',
    ] as const) {
      const result = await invokeTrusted({
        questions: [questionWithoutIds],
        status,
        reason: `${status} reason`,
      })
      expect(result).toMatchObject({
        status,
        answers: {},
        answer_ids: {},
        response: { status, answers: [], reason: `${status} reason` },
      })
      const block = AskUserQuestionTool.mapToolResultToToolResultBlockParam(
        result,
        'tool-use-1',
      )
      expect(block.content).toContain('No answers were submitted')
    }
  })
})

describe('AskUserQuestion HTML preview safety', () => {
  async function validate(preview: string) {
    setQuestionPreviewFormat('html')
    return AskUserQuestionTool.validateInput({
      questions: [
        {
          ...questionWithoutIds,
          options: [
            { ...questionWithoutIds.options[0], preview },
            questionWithoutIds.options[1],
          ],
        },
      ],
    })
  }

  test('accepts inert fragments with simple inline styles', async () => {
    expect(
      await validate(
        '<div style="color: #fff; padding: 4px"><pre>Safe</pre></div>',
      ),
    ).toEqual({ result: true })
  })

  test.each([
    '<div onclick="alert(1)">Click</div>',
    '<a href="javascript:alert(1)">Click</a>',
    '<a href="jav&#x61;script:alert(1)">Click</a>',
    '<a href="data:text/html,boom">Click</a>',
    '<a href="https://example.com">Remote</a>',
    '<a href="#local-fragment">Still interactive</a>',
    '<img src="https://example.com/tracker.png">',
    '<img/src="https://example.com/tracker.png">',
    '<picture><img/src="https://example.com/tracker.png"></picture>',
    '<div/onclick="alert(1)">Click</div>',
    '<td/background="https://example.com/tracker.png">Cell</td>',
    '<div style="background: url(https://example.com/a.png)">Remote</div>',
    '<div style="background: u/**/rl(https://example.com/a.png)">Remote</div>',
    '<div style="background: image-set(\"https://example.com/a.png\" 1x)">Remote</div>',
    '<iframe srcdoc="<p>nested</p>"></iframe>',
    '<object data="https://example.com/a"></object>',
    '<form action="https://example.com"><input></form>',
    '<base href="https://example.com">',
  ])('rejects active or resource-loading HTML: %s', async preview => {
    const result = await validate(preview)
    expect(result.result).toBe(false)
  })

  test('enforces the preview limit in UTF-8 bytes, not JavaScript characters', async () => {
    const result = await validate(`<div>${'界'.repeat(33_334)}</div>`)
    expect(result).toMatchObject({
      result: false,
      message: expect.stringContaining('UTF-8 bytes'),
    })
  })
})
