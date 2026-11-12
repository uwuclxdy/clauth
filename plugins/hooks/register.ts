// One Claude Code session bridged to the clauth daemon over its same-user unix
// socket, so a paired phone can answer the session's permission asks and steer
// it without owning the desk's keystrokes.
import { update } from 'claude-code'
import type { EngineInterface, Register, Timer } from 'claude-code'
import type { ClauthQueuedPrompt } from '../types/index'

// One event body, in bytes: past it the daemon refuses 413, so an ask whose
// body would exceed it is never held (the engine's own dialog draws instead).
const BODY_LIMIT = 1024 * 1024
// The poll cycle. The daemon parks a poll up to 20 s and `$.http.fetch` aborts
// at 30 s; one awaited fetch costs no hook budget, so the re-poll stays bounded
// on both sides.
const POLL_INTERVAL_MS = 1000
// A poll error waits this long before the next poll, so a down daemon never
// spins the loop.
const POLL_BACKOFF_MS = 5000
// The desk row's second line, leading text before the two permission buttons.
const LINE2_LEAD = '  Allow this? Answer here or on your phone.   '
// The AskUserQuestion row's second line, leading text before the one button.
const QUESTION_LINE2_LEAD = '  Answer on your phone, or here.   '
// The mid-turn check: the hidden `clauth` subcommand that prints both notes.
// The same run records the account this session spends now, and the
// token-speed reader attributes spend from that resolution's stamp, so the
// window bounds how much later than the next tool call a switch is recorded:
// a change with no later tool call waits for the next prompt. Inside a window
// a tool call pays no process, so N calls in one window cost one spawn
// instead of N.
const NOTE_CADENCE_MS = 10_000
// One run's bound: past it the note is dropped and the event settles without
// it, so a hung `clauth` cannot hold the tool result longer.
const NOTE_TIMEOUT_MS = 10_000
// The tool names Claude Code raises for an agent spawn (`is_agent_spawn_tool`
// in `src/hook_note.rs`).
const AGENT_SPAWN_TOOLS: readonly string[] = ['Task', 'Agent']
const NOTE_COMMAND = ['clauth', 'hook-profile-changed-note'] as const
// How long an Edit ask waits on its file. A `$` call in flight stops the hook
// budget, so nothing else ends a read that hangs on a dead mount or in a hook
// above; at the 4 MiB cap one takes 3-21 ms (measured 2026-11-07, 2.1.319).
const START_LINE_READ_BOUND_MS = 1000
// The rows an `append` wake is posted for. Where an organization seats its
// policy plugin (`cc-plugin-sec-default`, outermost for Team and Enterprise
// and on a machine with managed settings), attachment, hook-context and
// compaction rows, a Skill's tool messages and a managed plugin's notes go on
// past the person's plugins. `door` is pinned, so a hook narrowed to these
// doors is never in the chain for the others. The daemon tails the transcript
// file on each wake, so a row left out here reaches the phone with the next
// wake or at the session's end.
const APPEND_DOORS = ['prompt', 'command', 'response', 'tool-result', 'delivery', 'notice', 'queue'] as const

// The drops `$.prompt.submit` resolves before it accepts a prompt, while plugin
// input is held: the prompt never entered Claude Code's queue, so a later try
// can still land it. Every other drop value is final. The four texts were read
// from the Claude Code 2.1.322 binary on 2026-11-10.
const HELD_INPUT_DROPS: readonly unknown[] = [
  'the session was stopped and nobody has sent input since',
  'the host has ended its input',
  'the run is past its --max-budget-usd limit',
  'the run reached its --max-turns limit',
]
// Tries a prompt gets before it is reported dropped, so one Claude Code always
// refuses never holds the prompts behind it forever.
const MAX_PROMPT_TRIES = 3
// The `session.end` reasons the process survives: 2.1.322's types give both
// `clear` and `resume` as the process going on under another session id.
const CONTINUING_ENDS = new Set(['clear', 'resume'])

const SEQ = { plugin: 'clauth', key: 'seq' } as const
const HELD = { plugin: 'clauth', key: 'heldRow' } as const
const PROMPTS = { plugin: 'clauth', key: 'prompts' } as const

const encoder = new TextEncoder()

type HoldOutcome =
  | { type: 'allow' | 'deny'; from: 'desk' | 'device' }
  | { type: 'answers'; answers: Record<string, string>; notes: Record<string, string> }
  | { type: 'loss' }

// The stored prompt's one contract lives in `types/index.d.ts`.
type QueuedPrompt = ClauthQueuedPrompt

type PromptOutcome = 'delivered' | 'held' | 'removed'

type HeldEntry = {
  kind: 'decision' | 'answers'
  tool: string
  desc: string
  turn: string | null
  // The session the ask opened in, which its close must go out under even once
  // a `/clear` moved the module's own id on.
  session: string
  resolve: (o: HoldOutcome) => void
}

// Per-load runtime state. The values a hot reload must keep, the event
// sequence number and the prompt queue, live in `$.state` (SEQ, PROMPTS);
// everything here is re-established by the `session.start` that fires again on
// a reload.
let inert = false
let socketPath: string | null = null
let sessionId: string | null = null
// The id a continuing end (CONTINUING_ENDS) left behind: the process stays and
// `$.session.id()` moves on with no `session.start`, so the poll waits for it
// to leave this id, then registers the new one with the last `session.start`'s
// fields.
let endedSessionId: string | null = null
let startFields: Record<string, unknown> = {}
let interactive = false
let pollStarted = false
let pollCancel: Timer | null = null
let pollInFlight: Promise<void> | null = null
let sendChain: Promise<unknown> = Promise.resolve()
let nextPollAfter = 0
let turnId: string | null = null
// Phone prompts oldest first; only the head is ever tried. The same array is
// kept in `$.state` (PROMPTS), which `session.start` loads as the authority.
let promptQueue: QueuedPrompt[] = []
// Bumped at each session end: an attempt that settles under an older epoch
// belongs to a conversation that is gone.
let promptEpoch = 0
// The epoch whose drain is running, null when none is.
let drainEpoch: number | null = null
// A drain attempt that found one already running, so the running drain starts
// one more when it releases the flag and the session is still idle.
let drainAgain = false
let helds = new Map<string, HeldEntry>()
let interruptedTurn: string | null = null
let askCounter = 0
// The last cadence run per scope, keyed by agent id, the main thread's key empty.
let lastNoteAt = new Map<string, number>()

const bodyBytes = (body: string): number => encoder.encode(body).length

const oneLineJson = (value: unknown): string => {
  try {
    return JSON.stringify(value) ?? String(value)
  } catch {
    return String(value)
  }
}

// The first non-empty line, with an ellipsis when more content follows, so a
// multi-line command never grows the row past its two lines.
const collapseLines = (s: string): string => {
  const lines = s.replace(/\r\n?/g, '\n').split('\n')
  let i = 0
  while (i < lines.length && lines[i]!.trim() === '') i += 1
  if (i >= lines.length) return ''
  const first = lines[i]!
  const more = lines.slice(i + 1).some(l => l.trim() !== '')
  return more ? `${first}…` : first
}

// What line 1's `<what it acts on>` shows for a tool: Bash's command, a file
// tool's path, else the input as one-line JSON.
const describeInput = (tool: string, input: unknown): string => {
  if (tool === 'Bash') return collapseLines((input as { command?: string })?.command ?? '')
  if (tool === 'Write' || tool === 'Edit' || tool === 'Read') {
    return (input as { file_path?: string })?.file_path ?? ''
  }
  return oneLineJson(input)
}

// The first question's text, the AskUserQuestion row's line 1 description.
const firstQuestion = (input: unknown): string => {
  if (Array.isArray(input)) {
    const first = input[0] as { question?: unknown } | undefined
    if (first && typeof first.question === 'string') return collapseLines(first.question)
  }
  return ''
}

// The 1-based line `oldString` starts on in the bytes read; null when it is
// absent or, without `replaceAll`, matches twice, overlapping included. The
// engine searches a CRLF-folded copy, so this does too, and the BOM of a UTF-16
// file this read garbles shows as two U+FFFD (measured 2026-11-07, 2.1.319).
const startLineIn = (text: string, oldString: string, replaceAll: boolean): number | null => {
  if (text.startsWith('\u{fffd}\u{fffd}')) return null
  const folded = text.replaceAll('\r\n', '\n')
  const first = folded.indexOf(oldString)
  if (first === -1) return null
  if (!replaceAll && folded.indexOf(oldString, first + 1) !== -1) return null
  let line = 1
  for (let at = folded.indexOf('\n'); at !== -1 && at < first; at = folded.indexOf('\n', at + 1)) {
    line += 1
  }
  return line
}

// The pre-edit line of an Edit ask, or null: the number is a nicety on the ask,
// so this read may not fail the ask, and holds it for its bound at most.
async function editStartLine($: EngineInterface, input: unknown): Promise<number | null> {
  const { file_path: path, old_string: oldString, replace_all: replaceAll } = (input ?? {}) as {
    file_path?: unknown
    old_string?: unknown
    replace_all?: unknown
  }
  if (typeof path !== 'string' || typeof oldString !== 'string' || oldString === '') return null
  try {
    const text = await new Promise<string | null>((resolve, reject) => {
      const bound = $.clock.after(START_LINE_READ_BOUND_MS, () => resolve(null))
      $.fs.read(path).then(resolve, reject).finally(() => bound.cancel())
    })
    return text === null ? null : startLineIn(text, oldString, replaceAll === true)
  } catch {
    return null
  }
}

function mintAskId(): string {
  askCounter += 1
  return `ask-${askCounter}-${Math.random().toString(36).slice(2, 10)}`
}

function bridgeUrl(path: string): string {
  return `http://localhost${path}`
}

function fetchBridge($: EngineInterface, path: string, method: string, body?: string) {
  return $.http.fetch(bridgeUrl(path), {
    socketPath: socketPath as string,
    method,
    headers: body !== undefined ? { 'content-type': 'application/json' } : undefined,
    body,
  })
}

function nextSeq($: EngineInterface): Promise<number> {
  return update($, SEQ, cur => (cur ?? 0) + 1)
}

// Keeps the prompt queue in `$.state` the way SEQ is kept: every change writes
// the whole array, so a fresh load resumes the queue a dead module left.
async function savePrompts($: EngineInterface): Promise<void> {
  await update($, PROMPTS, () => promptQueue)
}

// Loads the stored queue at a fresh `session.start`: the state is the
// authority over the empty module state. An entry marked `submitting` was
// handed to Claude Code by the dead module and may already be in its queue, so
// it is reported dropped and never resubmitted.
async function loadPrompts($: EngineInterface): Promise<void> {
  const stored = (await $.state.get(PROMPTS)).value as unknown
  promptQueue = []
  if (!Array.isArray(stored)) return
  let changed = false
  for (const raw of stored) {
    if (typeof raw !== 'object' || raw === null) {
      changed = true
      continue
    }
    const entry = raw as { text?: unknown; id?: unknown; tries?: unknown; submitting?: unknown }
    if (typeof entry.text !== 'string') {
      changed = true
      continue
    }
    if (entry.submitting === true) {
      changed = true
      if (typeof entry.id === 'string') {
        void postEvent($, 'prompt', { prompt: entry.id, state: 'dropped', reason: 'unknown' })
      }
      continue
    }
    promptQueue.push({
      text: entry.text,
      ...(typeof entry.id === 'string' ? { id: entry.id } : {}),
      tries: typeof entry.tries === 'number' ? entry.tries : 0,
    })
  }
  if (changed) await savePrompts($)
}

// Runs `fn` as the next link of one ordered send chain: a link allocates its
// seq and fetches together, so concurrent sends never reorder and a 409 from
// the daemon's high-water mark cannot land on the wrong event. A rejected link
// must not wedge the chain, so the tail swallows every link's error.
function enqueueSend<T>(fn: () => Promise<T>): Promise<T> {
  const link = sendChain.then(fn)
  sendChain = link.then(
    () => undefined,
    () => undefined,
  )
  return link
}

// Posts one event under `session`, the id current at the call, and answers
// whether the daemon took it.
async function postEvent(
  $: EngineInterface,
  kind: string,
  fields: Record<string, unknown> = {},
  session: string | null = sessionId,
): Promise<boolean> {
  if (inert || session === null) return false
  return enqueueSend(async () => {
    const seq = await nextSeq($)
    const body = JSON.stringify({ session, seq, kind, ...fields })
    if (bodyBytes(body) > BODY_LIMIT) return false
    try {
      return (await fetchBridge($, '/event', 'POST', body)).ok
    } catch {
      // A refused or unreachable daemon must not stall the session: the event
      // is best-effort, the daemon re-syncs from its own state.
      return false
    }
  }).catch(() => false)
}

// Posts `ask_opened` and reads the daemon's hold answer; answers the session
// the ask opened under when the daemon holds it, null when the ask must not be
// held (over-bound body, unreachable socket, a non-ok reply, or no hold).
async function postAskOpened(
  $: EngineInterface,
  ask: string,
  tool: string,
  input: unknown,
  agentId: string | undefined,
): Promise<{ session: string } | null> {
  const session = sessionId
  if (session === null) return null
  // Started here and awaited in the link: a slow read ahead of a quick one
  // must not swap their asks in the send order.
  const startLine = tool === 'Edit' ? editStartLine($, input) : undefined
  return enqueueSend(async () => {
    const fields: Record<string, unknown> = { ask, tool, input }
    if (agentId !== undefined) fields.agent_id = agentId
    if (startLine !== undefined) fields.start_line = await startLine
    const seq = await nextSeq($)
    const body = JSON.stringify({ session, seq, kind: 'ask_opened', ...fields })
    if (bodyBytes(body) > BODY_LIMIT) return null
    try {
      const res = await fetchBridge($, '/event', 'POST', body)
      if (!res.ok) return null
      const parsed = JSON.parse(res.text) as { hold?: boolean }
      return parsed.hold === true ? { session } : null
    } catch {
      return null
    }
  }).catch(() => null)
}

// The oldest held ask, the one the desk row shows; its buttons answer that ask.
async function setHeldRow($: EngineInterface): Promise<void> {
  const first = helds.entries().next().value as [string, HeldEntry] | undefined
  const row = first
    ? { ask: first[0], kind: first[1].kind, tool: first[1].tool, desc: first[1].desc }
    : null
  await $.state.set(HELD, row)
}

async function resolveHold(
  $: EngineInterface,
  ask: string,
  outcome: HoldOutcome,
  postClose: boolean,
  aborted = false,
): Promise<void> {
  const entry = helds.get(ask)
  if (!entry) return
  helds.delete(ask)
  const by =
    aborted && interruptedTurn !== null && interruptedTurn === entry.turn ? 'interrupt' : 'desk'
  try {
    await setHeldRow($)
  } catch {
    // The drawing read is best-effort; the hold itself is over.
  }
  if (postClose) {
    await postEvent($, 'ask_closed', { ask, by }, entry.session)
  }
  entry.resolve(outcome)
}

// The desk answered (a Button press): close the ask at the desk and resolve.
function answerFromDesk($: EngineInterface, ask: string, decision: 'allow' | 'deny'): void {
  void resolveHold($, ask, { type: decision, from: 'desk' }, true)
}

// A phone answer for an ask: the daemon already closed it, so just resolve.
function answerFromDevice($: EngineInterface, ask: string, outcome: HoldOutcome): Promise<void> {
  return resolveHold($, ask, outcome, false)
}

// The desk took the AskUserQuestion back: release the hold and hand the engine
// its own question dialog.
function releaseToDesk($: EngineInterface, ask: string): void {
  void resolveHold($, ask, { type: 'loss' }, true)
}

async function handleAnswer($: EngineInterface, ask: string, cmd: Record<string, unknown>): Promise<void> {
  const entry = helds.get(ask)
  if (!entry) return
  if (entry.kind === 'decision') {
    // A permission ask releases only on a decision; the other form is ignored.
    if (cmd.decision !== 'allow' && cmd.decision !== 'deny') return
    await answerFromDevice($, ask, { type: cmd.decision, from: 'device' })
    return
  }
  // An AskUserQuestion releases only on answers, each value a string, and at
  // least one answer: an empty map holds nothing the engine can use.
  if (cmd.answers === null || typeof cmd.answers !== 'object') return
  const pairs = Object.entries(cmd.answers as Record<string, unknown>)
  if (pairs.length === 0) return
  if (!pairs.every(([, v]) => typeof v === 'string')) return
  // Built with `fromEntries`, never by assignment: a question whose exact text
  // is `__proto__` must survive as an own key.
  const answers = Object.fromEntries(pairs) as Record<string, string>
  // A note is optional: one that is not a non-empty string is dropped, never a
  // reason to hold back the answers beside it.
  const notePairs =
    cmd.notes !== null && typeof cmd.notes === 'object'
      ? Object.entries(cmd.notes as Record<string, unknown>).filter(
          ([, v]) => typeof v === 'string' && v !== '',
        )
      : []
  const notes = Object.fromEntries(notePairs) as Record<string, string>
  await answerFromDevice($, ask, { type: 'answers', answers, notes })
}

async function handleCommand($: EngineInterface, cmd: Record<string, unknown>): Promise<void> {
  if (typeof cmd !== 'object' || cmd === null) return
  switch (cmd.kind) {
    case 'prompt':
      if (typeof cmd.text === 'string') {
        // It joins the tail; an idle arrival starts the drain, which always
        // takes the head, so it never overtakes an older prompt.
        promptQueue.push({
          text: cmd.text,
          ...(typeof cmd.prompt === 'string' ? { id: cmd.prompt } : {}),
          tries: 0,
        })
        await savePrompts($)
        if (turnId === null) void drainPrompts($)
      }
      break
    case 'interrupt':
      if (turnId !== null) {
        interruptedTurn = turnId
        try {
          await $.turn.abort({ turnId })
        } catch {
          // The abort was refused; the tag must not outlive the interrupt.
          interruptedTurn = null
        }
      }
      break
    case 'answer':
      if (typeof cmd.ask === 'string') {
        await handleAnswer($, cmd.ask, cmd)
      }
      break
  }
}

// Submits the queue's prompts oldest first while the session is idle. One drain
// runs at a time, so a turn.complete landing while a submit is in flight never
// submits the same head twice; a drain attempt that finds one running is
// recorded and retried once the running drain releases its flag, so a
// completion landing during its final save is not lost. A delivery ends the
// drain: the delivered prompt's own turn.complete drains the next one, so this
// hook never holds the next turn open waiting on a prompt that only that turn
// can finish.
async function drainPrompts($: EngineInterface): Promise<void> {
  if (drainEpoch === promptEpoch) {
    drainAgain = true
    return
  }
  const epoch = promptEpoch
  drainEpoch = epoch
  drainAgain = false
  try {
    while (turnId === null && promptQueue.length > 0) {
      const head = promptQueue[0]
      if (head === undefined) break
      // Marked in flight before the submit, so a reload mid-submit sees it and
      // never resubmits a prompt Claude Code may already hold.
      head.submitting = true
      await savePrompts($)
      const outcome = await tryPrompt($, head.text)
      if (epoch !== promptEpoch) return
      if (outcome === 'held') {
        delete head.submitting
        head.tries += 1
        await savePrompts($)
        if (head.tries < MAX_PROMPT_TRIES) return
        await settleHead($, 'dropped', 'refused')
      } else if (outcome === 'delivered') {
        // A call that came while this submit was out is covered by the
        // delivered prompt's own turn.complete; replaying it now would submit
        // the next prompt before that turn starts.
        drainAgain = false
        await settleHead($, 'delivered')
        return
      } else {
        await settleHead($, 'dropped', 'removed')
      }
    }
  } catch {
    // A poll starts its drain unawaited, so a rejection here would land with no
    // handler; the queue keeps its head for the next turn.complete to retry.
  } finally {
    if (drainEpoch === epoch) drainEpoch = null
    if (drainAgain && epoch === promptEpoch && turnId === null) {
      drainAgain = false
      void drainPrompts($)
    }
  }
}

async function tryPrompt($: EngineInterface, text: string): Promise<PromptOutcome> {
  let result: unknown
  try {
    result = await submitPhonePrompt($, text)
  } catch {
    return 'held'
  }
  const drop = typeof result === 'object' && result !== null && 'drop' in result ? result.drop : undefined
  if (drop === undefined) return 'delivered'
  return HELD_INPUT_DROPS.includes(drop) ? 'held' : 'removed'
}

async function settleHead(
  $: EngineInterface,
  state: 'delivered' | 'dropped',
  reason?: 'refused' | 'removed',
): Promise<void> {
  const head = promptQueue.shift()
  if (head === undefined) return
  await savePrompts($)
  if (head.id === undefined) return
  void postEvent($, 'prompt', { prompt: head.id, state, ...(reason !== undefined ? { reason } : {}) })
}

// A phone prompt is the person's own words, so `asUser` has the model read it
// bare rather than framed as a plugin's message. A build refusing the option
// still delivers the prompt, framed, through one bare retry; a resolved drop is
// Claude Code's answer, so it gets none.
async function submitPhonePrompt($: EngineInterface, text: string): Promise<unknown> {
  try {
    return await $.prompt.submit({ text, asUser: true })
  } catch {
    return await $.prompt.submit({ text })
  }
}

async function onPollError($: EngineInterface): Promise<void> {
  const now = await $.clock.now()
  nextPollAfter = now + POLL_BACKOFF_MS
}

// One timer poll: the session's only poll, carrying prompts, interrupts and
// (redundantly) answers. A held check never pumps this loop — it waits on its
// own `/wait` — so a poll error or a dropped remote flag here touches no hold.
async function doPoll($: EngineInterface): Promise<void> {
  try {
    const session = await pollSession($)
    if (session === null) return
    // Captured at issue: a reply that lands after the conversation ended must
    // not dispatch its commands into the next one.
    const epoch = promptEpoch
    let res
    try {
      res = await fetchBridge(
        $,
        `/poll?session=${encodeURIComponent(session)}&prompt_reports=1`,
        'GET',
      )
    } catch {
      await onPollError($)
      return
    }
    if (!res.ok) {
      await onPollError($)
      return
    }
    let reply: { remote?: boolean; commands?: Record<string, unknown>[] }
    try {
      reply = JSON.parse(res.text) as typeof reply
    } catch {
      await onPollError($)
      return
    }
    nextPollAfter = 0
    if (reply.remote !== true) return
    for (const cmd of reply.commands ?? []) {
      // Checked per command: an earlier command can wait while the
      // conversation ends, and none after it belongs to the next one.
      if (epoch !== promptEpoch) return
      try {
        await handleCommand($, cmd)
      } catch {
        // A refused command must not drop the ones behind it in the reply.
      }
    }
  } catch {
    // A defect in the poll handler must not reject the pump loop.
    await onPollError($).catch(() => {})
  }
}

// The id this poll runs under, null to skip the tick. An id `$.session.id()`
// names that is neither the registered nor the ended one is the process's
// next conversation: its `start` goes out first, and it is polled under only
// once the daemon took that `start`. A read that fails keeps the registered id.
async function pollSession($: EngineInterface): Promise<string | null> {
  let current: string
  try {
    current = await $.session.id()
  } catch {
    return sessionId
  }
  if (current === sessionId || current === endedSessionId) return sessionId
  if (!(await postEvent($, 'start', startFields, current))) {
    await onPollError($)
    return null
  }
  // A continuing end can land while that `start` was out and end this very id:
  // the next tick waits for the id after it.
  if (current === endedSessionId) return null
  sessionId = current
  endedSessionId = null
  return current
}

// The session a continuing end left behind stops being spoken for here, unless
// the poll already moved to the next one.
function leaveEndedSession(ended: string): void {
  endedSessionId = ended
  if (sessionId === ended) sessionId = null
}

// The timer's single in-flight poll: at most one timer poll parks at a time.
function pollOnce($: EngineInterface): Promise<void> {
  if (pollInFlight) return pollInFlight
  pollInFlight = doPoll($).finally(() => {
    pollInFlight = null
  })
  return pollInFlight
}

// One wait on one ask: a single `/wait` fetch under the session the ask opened
// in, parked at the daemon until that ask settles. The module's own id may have
// moved on (a `/clear` keeps the process under another), and the ask still
// belongs to the session that opened it. The awaited fetch pauses the hook
// budget, so a hold outlives the 10 s bound. Every non-settle reads as a loss
// for this ask alone.
async function waitOnce($: EngineInterface, ask: string, session: string): Promise<void> {
  let res
  try {
    res = await fetchBridge(
      $,
      `/wait?session=${encodeURIComponent(session)}&ask=${encodeURIComponent(ask)}`,
      'GET',
    )
  } catch {
    await resolveHold($, ask, { type: 'loss' }, true)
    return
  }
  if (!res.ok) {
    await resolveHold($, ask, { type: 'loss' }, true)
    return
  }
  let reply: { remote?: boolean; answer?: Record<string, unknown> | null; closed?: boolean }
  try {
    reply = JSON.parse(res.text) as typeof reply
  } catch {
    await resolveHold($, ask, { type: 'loss' }, true)
    return
  }
  if (reply.remote !== true) {
    await resolveHold($, ask, { type: 'loss' }, true)
    return
  }
  if (reply.answer !== null && reply.answer !== undefined) {
    // A device answer, form-matched exactly as a poll answer is.
    await handleAnswer($, ask, reply.answer)
    if (helds.has(ask)) {
      // The answer's form did not match this hold, which only a daemon or mod
      // of another version delivers: the daemon refuses the wrong form and
      // empty or non-string answers. The daemon already closed the ask, so
      // resolve it as a loss rather than re-waiting on a closed ask forever.
      await resolveHold($, ask, { type: 'loss' }, true)
    }
    return
  }
  if (reply.closed === true) {
    // Closed with no answer: the desk closed it (the press already resolved the
    // hold, so resolveHold no-ops) or the session ended — either way a still-
    // held ask resolves as a loss.
    await resolveHold($, ask, { type: 'loss' }, true)
    return
  }
  // The bound elapsed with the ask still open: wait again.
}

// Holds `ask` under the session it opened in until it resolves (desk, phone,
// loss, or an abort), waiting on that ask's own `/wait` so the hook budget stays
// paused on an awaited fetch. The race against the outcome promise makes a desk
// press or an abort end the wait without waiting out the parked fetch; its
// later reply is then ignored.
async function holdUntilSettled(
  $: EngineInterface,
  ask: string,
  session: string,
  kind: HeldEntry['kind'],
  tool: string,
  desc: string,
  turn: string | null,
  signal: AbortSignal | undefined,
): Promise<HoldOutcome> {
  let settled = false
  const hp = new Promise<HoldOutcome>(resolve => {
    // An abort resolves only this ask; a wait's loss also resolves only its own
    // ask.
    const onAbort = () => {
      void resolveHold($, ask, { type: 'loss' }, true, true)
    }
    signal?.addEventListener('abort', onAbort, { once: true })
    helds.set(ask, {
      kind,
      tool,
      desc,
      turn,
      session,
      resolve: o => {
        settled = true
        signal?.removeEventListener('abort', onAbort)
        resolve(o)
      },
    })
    // The signal may have aborted while the ask_opened round trip was still in
    // flight; a listener on an already-aborted signal never fires, so resolve
    // the hold now.
    if (signal?.aborted) onAbort()
  })
  try {
    await setHeldRow($)
  } catch {
    // The row is a convenience; the hold stands even when it cannot draw.
  }
  while (!settled && !signal?.aborted) {
    await Promise.race([hp, waitOnce($, ask, session)])
  }
  return hp
}

function startPollLoop($: EngineInterface): void {
  if (pollStarted) return
  pollStarted = true
  pollCancel = $.clock.every(POLL_INTERVAL_MS, async () => {
    if (inert) return
    // The timer owns the session's one poll; a hold never shares it.
    const now = await $.clock.now()
    if (now < nextPollAfter) return
    void pollOnce($)
  })
}

function stopPollLoop(): void {
  pollCancel?.cancel()
  pollCancel = null
  pollStarted = false
}

// The scope one note fire speaks for.
type NoteScope = {
  session_id: string
  tool_name: string
  agent_id?: string
}

// The window one fire belongs to: a subagent's calls never spend the main
// thread's window or another subagent's. The session's own windows start
// over at each `session.start` and at `session.end` (a `/clear` ends the
// conversation with no `session.start`), so the loop alone names the scope.
function noteWindowKey(agentId: string | undefined): string {
  return agentId ?? ''
}

// The classic hook input the note command reads, built from the tool call:
// `clauth`'s own parser keeps these fields, and the scope is the call's, so a
// subagent's note lands on the subagent's record. No transcript path rides
// along: the engine hands a hook none, and the command takes a subagent
// record's from the main record its `SessionStart` fire stamped.
function noteInput(e: NoteScope): string {
  return JSON.stringify({
    session_id: e.session_id,
    hook_event_name: 'PostToolUse',
    tool_name: e.tool_name,
    ...(e.agent_id !== undefined ? { agent_id: e.agent_id } : {}),
  })
}

// The command's classic envelope as text for the model, or nothing: it prints
// nothing when no note earned the fire, and every other answer is silence too.
function noteContext(stdout: string): string | null {
  try {
    const value = JSON.parse(stdout) as {
      hookSpecificOutput?: { hookEventName?: string; additionalContext?: unknown }
    }
    const out = value.hookSpecificOutput
    if (out === undefined || out.hookEventName !== 'PostToolUse') return null
    if (typeof out.additionalContext !== 'string') return null
    return out.additionalContext === '' ? null : out.additionalContext
  } catch {
    return null
  }
}

// One cadence window's run of the note command: the note's text, or nothing.
// A command that cannot start or overruns is silence, as the subcommand's own
// contract is, so a broken install never breaks the tool result it rides on.
async function runNote($: EngineInterface, e: NoteScope): Promise<string | null> {
  try {
    const run = await $.process.run(NOTE_COMMAND, {
      stdin: noteInput(e),
      timeoutMs: NOTE_TIMEOUT_MS,
    })
    return run.exitCode === 0 ? noteContext(run.stdout) : null
  } catch {
    return null
  }
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    // The note windows belong to the session: a fresh one starts open.
    lastNoteAt.clear()
    const os = await $.env.get('OS')
    const home = await $.env.get('HOME')
    if (os === 'Windows_NT' || !home || !home.startsWith('/')) {
      inert = true
      return next(e)
    }
    socketPath = `${home}/.clauth/bridge.sock`
    interactive = e.isInteractive && e.surface !== null
    sessionId = await $.session.id()
    interruptedTurn = null
    await loadPrompts($)
    await $.state.set(HELD, null)
    startFields = { cwd: e.cwd, interactive: e.isInteractive, prompt_reports: true }
    void postEvent($, 'start', startFields)
    startPollLoop($)
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    lastNoteAt.clear()
    // A prompt queued for this conversation must not reach the next one. The
    // daemon reports each dropped prompt itself once it takes the end. The
    // epoch moves before the state write, so a poll reply landing while it
    // saves finds an epoch already past and runs none of its commands.
    promptQueue = []
    promptEpoch += 1
    // The ending id is the engine's, since the poll may already speak for the
    // next one; with none registered there is nothing to end.
    const ended = sessionId === null ? null : e.sessionId
    // A continuing end keeps the process, so the poll goes on to register its
    // next id; any other end is the process leaving.
    const processContinues = CONTINUING_ENDS.has(e.reason)
    if (processContinues) leaveEndedSession(e.sessionId)
    await savePrompts($)
    if (!inert) {
      if (!processContinues) stopPollLoop()
      // Await the `end` post alone (the engine's own short bound caps it), so
      // the daemon learns the end now rather than from its silence sweep.
      await postEvent($, 'end', {}, ended)
    }
    return next(e)
  })

  on('session.append', { door: APPEND_DOORS }, async ($, e, next) => {
    if (!inert) {
      void postEvent($, 'append', {
        uuid: e.uuid,
        ...(e.agentId !== undefined ? { agent_id: e.agentId } : {}),
      })
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  on('turn.start', async ($, e, next) => {
    if (!inert && e.agentId === undefined) {
      turnId = e.turnId
      void postEvent($, 'turn_start', { turn: e.turnId })
    }
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    // A subagent's run ends as its turn does: its note window goes with it.
    // Nothing else bounds the map but the sweep, and a stale entry could only
    // withhold a run a later fire owes.
    if (e.agentId !== undefined) lastNoteAt.delete(noteWindowKey(e.agentId))
    if (!inert && e.agentId === undefined) {
      turnId = null
      interruptedTurn = null
      void postEvent($, 'turn_end', { turn: e.turnId })
      await drainPrompts($)
    }
    return next(e)
  })

  // The mid-turn note rides a tool call's result, not `classic.PostToolUse`:
  // where an organization seats its policy plugin every classic event goes on
  // past the person's plugins, so a classic hook here would never run there.
  // A result's `context` reaches the model as a PostToolUse hook's
  // `additionalContext` did, one reminder after the result.
  on('tool.call', async ($, e, next) => {
    const settled = await next(e)
    // PostToolUse fired for an answered call alone: a refusal or an error
    // carries no note and spends no window.
    if (settled.deny !== undefined || settled.isError === true) return settled
    // A fire that cannot name its session runs nothing, so it must not spend
    // the window it would have run in: the read comes before the stamp.
    const sid = await $.session.id()
    // A main-scope agent-spawn call is what the headroom nudge is computed
    // on, and a skipped fire never replays it, so it runs the command
    // whatever the window says and leaves the window where it stood.
    const spawn = e.agentId === undefined && AGENT_SPAWN_TOOLS.includes(e.tool)
    if (!spawn) {
      const key = noteWindowKey(e.agentId)
      const now = await $.clock.now()
      const lastAt = lastNoteAt.get(key)
      if (lastAt !== undefined && now - lastAt < NOTE_CADENCE_MS) return settled
      // A scope whose run never completes ages out here, so the map holds
      // only the scopes that ran inside one window.
      for (const [stale, at] of lastNoteAt) {
        if (now - at >= NOTE_CADENCE_MS) lastNoteAt.delete(stale)
      }
      // Stamped before the run's own awaits, so tools settling together share
      // the window instead of each spawning.
      lastNoteAt.set(key, now)
    }
    const note = await runNote($, {
      session_id: sid,
      tool_name: e.tool,
      ...(e.agentId !== undefined ? { agent_id: e.agentId } : {}),
    })
    if (note === null) return settled
    return { ...settled, context: [...(settled.context ?? []), note] }
  }).catch(($, e, next) => next(e))

  on(
    'tool.check',
    async ($, e, next) => {
      // AskUserQuestion's own permission check runs inside its tool call's
      // `next`, after the AskUserQuestion `tool.call` hook below let the call
      // through; the check's `ask` is the question dialog
      // itself, which a hook's `allow` cannot dismiss (the 2.1.322 types).
      // Holding it here would put the same question to the person twice
      // (measured 2026-11-11 at the desk).
      if (inert || e.tool === 'AskUserQuestion') return next(e)
      const v = await next(e)
      if (v.decision !== 'ask') return v
      if (!interactive) return v
      const ask = mintAskId()
      const turn = turnId
      const opened = await postAskOpened($, ask, e.tool, e.input, e.agentId)
      if (opened === null) return v
      const outcome = await holdUntilSettled(
        $,
        ask,
        opened.session,
        'decision',
        e.tool,
        describeInput(e.tool, e.input),
        turn,
        next.signal,
      )
      if (outcome.type === 'loss') return v
      if (outcome.type === 'allow') return { decision: 'allow' }
      if (outcome.type === 'deny') return { decision: 'deny' }
      return v
    },
  ).catch(async ($, e, next) => (next.called ? next(e) : { decision: 'ask' }))

  on(
    'tool.call',
    { tool: 'AskUserQuestion' },
    async ($, e, next) => {
      if (inert || !interactive) return next(e)
      const ask = mintAskId()
      const turn = turnId
      const opened = await postAskOpened($, ask, 'AskUserQuestion', e.questions, e.agentId)
      if (opened === null) return next(e)
      const outcome = await holdUntilSettled(
        $,
        ask,
        opened.session,
        'answers',
        'AskUserQuestion',
        firstQuestion(e.questions),
        turn,
        next.signal,
      )
      if (outcome.type === 'answers') {
        // The engine's output-shape check refuses a result without the
        // tool's own `questions` (measured 2026-11-06, 2.1.319). A note reaches
        // the model only as its question's `annotations` entry (same run).
        const noted = Object.entries(outcome.notes)
        const result = { questions: e.questions, answers: outcome.answers }
        if (noted.length === 0) return { result }
        const annotations = Object.fromEntries(noted.map(([q, notes]) => [q, { notes }]))
        return { result: { ...result, annotations } }
      }
      return next(e)
    },
  ).catch(($, e, next) => next(e))

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const { value: row } = await $.state.get(HELD)
    if (!row) return next(e)
    const { Box, Text, Button } = $.ui.resolve(e)
    const line1 = `  ${row.tool}  ${row.desc}`
    const line2 =
      row.kind === 'decision'
        ? Box({
            children: [
              Text({ children: LINE2_LEAD }),
              Button({ key: 'allow', plain: true, hotkey: '1', label: 'Allow', onPress: () => answerFromDesk($, row.ask, 'allow') }),
              Text({ children: '   ' }),
              Button({ key: 'deny', plain: true, hotkey: '2', label: 'Deny', onPress: () => answerFromDesk($, row.ask, 'deny') }),
            ],
          })
        : Box({
            children: [
              Text({ children: QUESTION_LINE2_LEAD }),
              Button({ key: 'answer-here', plain: true, hotkey: '1', label: 'Answer here', onPress: () => releaseToDesk($, row.ask) }),
              Text({ children: '   Esc cancels' }),
            ],
          })
    return Box({
      flexDirection: 'column',
      children: [Text({ wrap: 'truncate-end', children: line1 }), line2],
    })
  })
}
