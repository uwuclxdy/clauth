// One Claude Code session bridged to the clauth daemon over its same-user unix
// socket, so a paired phone can answer the session's permission asks and steer
// it without owning the desk's keystrokes. Beside that, a pane lists the
// session's clauth delegates off clauth's job files, with each one's
// transcript, a stop, and a notice as each one finishes.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register, Timer, ToolCallResult } from 'claude-code'
import type {
  ClauthDelegateRow,
  ClauthDelegateView,
  ClauthDelegateWord,
  ClauthQueuedPrompt,
} from '../types'

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

// The tool names a clauth `delegate` call arrives under, each with the server
// its `monitor` is called on: the plugin's own server, or one the person
// registered as `clauth` (the PostToolUse matcher in hooks.json takes both).
// The tool-name spelling is tried first: it is the one a live stop press was
// measured to resolve, and the `/mcp` spelling the engine also accepts.
const DELEGATE_TOOLS = ['mcp__plugin_clauth_clauth__delegate', 'mcp__clauth__delegate'] as const
type DelegateTool = (typeof DELEGATE_TOOLS)[number]
const MONITOR_SERVERS: Readonly<Record<DelegateTool, string>> = {
  mcp__plugin_clauth_clauth__delegate: 'plugin:clauth:clauth',
  mcp__clauth__delegate: 'clauth',
}
const TOOL_SERVERS: Readonly<Record<DelegateTool, string>> = {
  mcp__plugin_clauth_clauth__delegate: 'plugin_clauth_clauth',
  mcp__clauth__delegate: 'clauth',
}
const DELEGATES_PANE = 'clauth-delegates'
const DELEGATES_TITLE = 'clauth delegates'
const DELEGATES_COMMAND = 'delegates'
// The job store's read cadence, kept only while a delegate of this session is
// live: new ones come from a `delegate` call, which restarts it.
const DELEGATES_TICK_MS = 2000
// The hint line's own store read: the first draw after this long asks for one
// while the ticks are stopped, so a job minted outside a delegate call lands
// without a store listing on every draw.
const HINT_SCAN_MS = 1000
// A blocking run's record: liveness only, beside the collectable `<id>.json`.
const LIVE_SUFFIX = '.live.json'
const LIVE_WORDS: readonly ClauthDelegateWord[] = ['running', 'blocking']
// How many scans a live row's record may be absent before the row settles. A
// collect renames the record away before its delivery ledger lands, so one
// scan inside that window must not read the delivery as `ended`.
const DELEGATE_ABSENT_SCANS = 3
// A live record whose file has sat still this long is either a silent run or
// a dead one; only its owner's liveness tells them apart. The heartbeat
// rewrites the record at most every couple of seconds, so a still file past
// this bound is safe to probe, and a probe runs once a minute at most.
const DELEGATE_OWNER_STILL_MS = 60_000
const DELEGATE_OWNER_PROBE_MS = 60_000
const DELEGATE_OWNER_PROBE_TIMEOUT_MS = 2000
// A running record with no owner pid is a corpse once its own stamps are
// silent past clauth's running TTL: silence is the only rule such a record
// answers.
const DELEGATE_OWNERLESS_SILENT_MS = (24 * 60 * 60 + 600) * 1000
// Claude Code's project folder names a slug past this length by its first
// characters plus the path's hash, so a longer name finds no transcript.
const PROJECT_SLUG_MAX = 200
// A transcript is read from its end: one `.jsonl` passes 4 MiB, and one tool
// result can fill a line of its own, so the bound is bytes, not lines.
const TRANSCRIPT_TAIL_BYTES = 512 * 1024
const TRANSCRIPT_READ_TIMEOUT_MS = 5000
const TRANSCRIPT_LINES = 200
const TRANSCRIPT_LINE_CHARS = 400
const NO_TRANSCRIPT = 'no transcript to show'
const EMPTY_TRANSCRIPT = 'nothing in the transcript yet'

// The rows' fields follow clauth's job record: bump a shape when they change,
// so a reload declines what the older module stored.
const DELEGATES = atom({ plugin: 'clauth', key: 'delegates' } as const, [] as ClauthDelegateRow[], {
  shape: 'delegates-1',
})
const DELEGATE_VIEW = atom(
  { plugin: 'clauth', key: 'delegateView' } as const,
  null as ClauthDelegateView | null,
  { shape: 'delegate-view-1' },
)

const encoder = new TextEncoder()

type HoldOutcome =
  | { type: 'allow' | 'deny'; from: 'desk' | 'device' }
  | { type: 'answers'; answers: Record<string, string>; notes: Record<string, string> }
  | { type: 'loss' }
  // The daemon's clean release of a held ask to the desk: the daemon keeps
  // listing the ask, read-only, until its close. A call's ask closes once the
  // call settles, after the desk's dialog and its run; a bare query opens no
  // dialog and closes at once.
  | { type: 'released' }

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
// Permission asks the daemon took but does not hold — taken unheld, or released
// back to the desk — by ask id, each with the session it opened in and the tool
// call (`tool_use_id`) whose settling closes it.
let unheldAsks = new Map<string, { session: string; call: string }>()
let interruptedTurn: string | null = null
let askCounter = 0
// The last cadence run per scope, keyed by agent id, the main thread's key empty.
let lastNoteAt = new Map<string, number>()
// The delegates pane's runtime. Each job file's last-read stamp (mtime and
// size) lets an unchanged record, another session's included, be read once
// rather than every tick; the viewed transcript's stamp does the same for it.
let jobsDir: string | null = null
let projectsDir: string | null = null
let delegatesTimer: Timer | null = null
let delegateScanInFlight: Promise<void> | null = null
let delegateCallsInFlight = 0
let jobFileStamps = new Map<string, string>()
let viewStamp: string | null = null
let monitorServer: string = MONITOR_SERVERS.mcp__plugin_clauth_clauth__delegate
let monitorTool: DelegateTool | null = null
// The live rows whose record file last moved, when, its owner, and the newest
// stamp the record's own fields carry: the owner probe's throttle, the pid it
// asks about, and the silence an ownerless record is judged by.
let recordWatches = new Map<string, { movedAt: number; ownerPid: number; probedAt: number; silentAt: number }>()
// The live rows whose record was already absent at the previous scan.
const delegateAbsentScans = new Map<string, number>()
// Job ids a cancel named (a blocking call's own settle, or clauth's reply to
// a stop press), settled by the scan that finds their record gone.
const cancelledJobs = new Set<string>()
// The session ids a `/clear` ended. Their rows die with `$.state`, so a run
// still live under one is adopted again while it runs.
const priorSessions = new Set<string>()
// The live rows a `/clear` took out of `$.state`, held until the store read
// that merges them back commits its rows.
let carriedRows: ClauthDelegateRow[] = []
// When the hint line last asked for a store read.
let hintScanAt = 0
// Whether this host's `ps` answers the owner probe's form: null until asked.
let psFormWorks: boolean | null = null
// Whether each surface docks a pane beside the transcript, the fact every
// drawing's viewport carries. Kept per surface: one session may draw on
// several at once, and a single value would be whichever surface drew last.
const surfaceLayouts = new Map<string, boolean>()
// Bumped at each session end: a store read that settles under an older epoch
// belongs to a conversation that is gone.
let sessionEpoch = 0
let sessionEnding = false
// The delegate call whose one unasked open is spent. A call that finds the
// pane already open spends it too, so a close during the call is not undone
// when the call settles.
let delegateCallGen = 0
let autoOpenGen = -1
// When this process's session started: a record first seen finished but whose
// run ended before this was told in its own conversation.
let deliveredSince = 0

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

// Posts `ask_opened` and reads the daemon's hold answer: the session the ask
// opened under and whether the daemon holds it, once the daemon took the ask,
// which every ok reply means (`ask_opened` arm in `src/daemon/bridge.rs`); null
// when it never did (no session, over-bound body, unreachable socket, a non-ok
// reply). A reply that does not parse holds nothing.
async function postAskOpened(
  $: EngineInterface,
  ask: string,
  tool: string,
  input: unknown,
  agentId: string | undefined,
): Promise<{ session: string; held: boolean } | null> {
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
    let res
    try {
      res = await fetchBridge($, '/event', 'POST', body)
    } catch {
      return null
    }
    if (!res.ok) return null
    return { session, held: holdAnswer(res.text) }
  }).catch(() => null)
}

function holdAnswer(reply: string): boolean {
  try {
    return (JSON.parse(reply) as { hold?: boolean }).hold === true
  } catch {
    return false
  }
}

// An ask the daemon lists but does not hold — taken unheld, or a held one it
// released to the desk — goes to Claude Code's own dialog, so only the desk
// settles it, and the daemon lists it until it is closed or the session ends.
// A query opens no dialog and closes at once; a real call's ask closes once
// that call settles (`closeUnheldAsks`).
function closeUnheldAskWhenSettled(
  $: EngineInterface,
  ask: string,
  session: string,
  call: string | undefined,
): void {
  if (call === undefined) closeUnheldAsk($, ask, session)
  else unheldAsks.set(ask, { session, call })
}

function closeUnheldAsk($: EngineInterface, ask: string, session: string): void {
  void postEvent($, 'ask_closed', { ask, by: 'desk' }, session)
}

// The engine raises `tool.check` inside the `tool.call` chain's `next` (the
// 2.1.322 types), so a call whose hooks did not fail before `next` has settled
// every check it raised; the `tool.call` hooks in this file let no await before
// `next` reject.
function closeUnheldAsks($: EngineInterface, call: string | undefined): void {
  if (call === undefined) return
  for (const [ask, unheld] of unheldAsks) {
    if (unheld.call !== call) continue
    unheldAsks.delete(ask)
    closeUnheldAsk($, ask, unheld.session)
  }
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

// The daemon released a held ask to the desk: it keeps listing the ask,
// read-only, until its close, so the close waits for the call's settle. The
// hook that held the ask owns the close: a bare check query opens no dialog and
// closes at once, and a call's ask once the call settles, after its run.
function takeDaemonRelease($: EngineInterface, ask: string): Promise<void> {
  return resolveHold($, ask, { type: 'released' }, false)
}

// One wait on one ask: a single `/wait` fetch under the session the ask opened
// in, parked at the daemon until that ask settles. The module's own id may have
// moved on (a `/clear` keeps the process under another), and the ask still
// belongs to the session that opened it. The awaited fetch pauses the hook
// budget, so a hold outlives the 10 s bound. Every failed wait reads as a loss
// for this ask alone; the daemon's clean release hands the ask to the desk,
// whose settle closes it (`takeDaemonRelease`).
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
    // Only the clean release of a still-open ask defers the close; a closed
    // ask or a queued answer beside `remote: false` is no release and closes
    // at once, as every non-settle does. This daemon sends an answer only
    // beside `closed: true`, so the answer half guards against another
    // version's bare answer, not this daemon's.
    if (reply.closed === true || (reply.answer !== null && reply.answer !== undefined)) {
      await resolveHold($, ask, { type: 'loss' }, true)
      return
    }
    await takeDaemonRelease($, ask)
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

type JsonObject = Record<string, unknown>

const isLive = (word: ClauthDelegateWord): boolean => LIVE_WORDS.includes(word)

// The layout fact a drawing hands over, keyed by the surface that drew; the
// events that open a pane carry none of their own.
const noteLayout = (surface: string, viewport: { isFullscreen?: boolean } | undefined): void => {
  if (viewport?.isFullscreen === true) surfaceLayouts.set(surface, true)
  else if (viewport?.isFullscreen === false) surfaceLayouts.set(surface, false)
}

const textOf = (value: unknown): string => (typeof value === 'string' ? value : '')

const epochMsOf = (value: unknown): number =>
  typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : 0

const objectOf = (value: unknown): JsonObject | null =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as JsonObject) : null

// A path's 32-bit hash, the tail Claude Code appends to a long project slug.
const projectHash = (cwd: string): number => {
  let hash = 0
  for (let i = 0; i < cwd.length; i += 1) hash = ((hash << 5) - hash + cwd.charCodeAt(i)) | 0
  return hash
}

// Claude Code's project folder for a working directory: every character
// outside [a-zA-Z0-9] becomes `-`, and a slug past 200 characters keeps its
// first 200 plus the path's hash in base 36.
const projectSlug = (cwd: string): string => {
  const slug = cwd.replace(/[^a-zA-Z0-9]/g, '-')
  if (slug.length <= PROJECT_SLUG_MAX) return slug
  return `${slug.slice(0, PROJECT_SLUG_MAX)}-${Math.abs(projectHash(cwd)).toString(36)}`
}

// `humanize_duration` (src/usage/fetch.rs) for a run's length, which reads
// `0s` at its start rather than `now`.
function formatElapsed(secs: number): string {
  if (secs <= 0) return '0s'
  const mins = Math.floor(secs / 60)
  const hours = Math.floor(mins / 60)
  const days = Math.floor(hours / 24)
  if (days > 0) return `${days}d ${hours % 24}h`
  if (hours > 0) return `${hours}h ${mins % 60}m`
  if (secs >= 300) return `${mins}m`
  if (mins === 0) return `${secs}s`
  return secs % 60 === 0 ? `${mins}m` : `${mins}m ${secs % 60}s`
}

// A live run's time so far, a finished one's whole length; null when the
// record named no start.
function elapsedOf(row: ClauthDelegateRow, now: number): string | null {
  if (row.startedAt === 0) return null
  const end = isLive(row.word) ? now : row.endedAt
  return formatElapsed(Math.floor((end - row.startedAt) / 1000))
}

// The words a row and its view's head show: id, account, state, time.
function delegateSummary(row: ClauthDelegateRow, now: number): string {
  const elapsed = elapsedOf(row, now)
  return [row.id, row.profile, row.word, ...(elapsed === null ? [] : [elapsed])].join('  ')
}

async function readJsonObject($: EngineInterface, path: string): Promise<JsonObject | null> {
  try {
    return objectOf(JSON.parse(await $.fs.read(path)))
  } catch {
    return null
  }
}

async function fileStamp($: EngineInterface, path: string): Promise<string | null> {
  try {
    const stat = await $.fs.stat(path)
    return `${stat.mtimeMs}:${stat.size}`
  } catch {
    return null
  }
}

// The word a record names: a live run, a stop at the host's exit (the
// in-process record and the sweep's tombstone alike), a cancel flagged beside
// `done`, the sweep's crash tombstone, then plain `done`.
function delegateWordOf(record: JsonObject, isBlocking: boolean): ClauthDelegateWord {
  const state = textOf(record.state)
  if (state === 'running') return isBlocking ? 'blocking' : 'running'
  if (record.stopped === true) return 'stopped'
  if (objectOf(record.envelope)?.cancelled === true) return 'cancelled'
  if (record.crashed === true) return 'orphaned'
  return 'done'
}

// When a finished record's run ended: its `done_at`, else its start plus the
// envelope's run length, else now, the moment it was seen finished.
function endedAtOf(record: JsonObject, now: number): number {
  const doneAt = epochMsOf(record.done_at)
  if (doneAt !== 0) return doneAt
  const startedAt = epochMsOf(record.started_at)
  const elapsedSecs = epochMsOf(objectOf(record.envelope)?.elapsed_secs)
  return startedAt !== 0 && elapsedSecs !== 0 ? startedAt + elapsedSecs * 1000 : now
}

function delegateRowOf(record: JsonObject, isBlocking: boolean, now: number): ClauthDelegateRow {
  const word = delegateWordOf(record, isBlocking)
  const sessionId = textOf(record.session_id)
  const cwd = textOf(record.cwd)
  return {
    id: textOf(record.job_id),
    profile: textOf(record.profile),
    word,
    startedAt: epochMsOf(record.started_at),
    endedAt: isLive(word) ? 0 : endedAtOf(record, now),
    tail: textOf(record.tail),
    transcript:
      sessionId === '' || cwd === '' || projectsDir === null
        ? null
        : `${projectsDir}/${projectSlug(cwd)}/${sessionId}.jsonl`,
    isNoticed: false,
  }
}

// One record read this scan, with the file stamp the read answers, so the
// stamp can be committed only once the rows it folded into are stored.
type JobRead = { record: JsonObject; isBlocking: boolean; file: string; stamp: string }

// The job files written since their last read. A file that does not parse
// (half-written, or not a record) keeps no stamp, so the next tick reads it
// again.
async function readChangedJobs(
  $: EngineInterface,
  dir: string,
  files: readonly { name: string; mtimeMs: number; size: number }[],
  names: ReadonlySet<string>,
): Promise<JobRead[]> {
  const reads: JobRead[] = []
  for (const file of files) {
    const isBlocking = file.name.endsWith(LIVE_SUFFIX)
    if (!isBlocking && !file.name.endsWith('.json')) continue
    // A blocking run handed off to the background holds both spellings for a
    // moment; the collectable one is the run's from then on.
    if (isBlocking && names.has(`${file.name.slice(0, -LIVE_SUFFIX.length)}.json`)) continue
    const stamp = `${file.mtimeMs}:${file.size}`
    if (jobFileStamps.get(file.name) === stamp) continue
    const record = await readJsonObject($, `${dir}/${file.name}`)
    if (record === null || textOf(record.job_id) === '') continue
    reads.push({ record, isBlocking, file: file.name, stamp })
  }
  return reads
}

// A live row whose record left the store: a stop the call's own settle named,
// collected (its delivery ledger stands), a blocking run returned through its
// call, or gone with no result. A record that is merely absent waits out the
// grace: a collect renames it away before the ledger lands, and only the
// ledger's word may settle that.
async function settleGoneDelegate(
  $: EngineInterface,
  dir: string,
  row: ClauthDelegateRow,
  names: ReadonlySet<string>,
  now: number,
): Promise<ClauthDelegateRow> {
  if (names.has(`${row.id}.json`) || names.has(`${row.id}${LIVE_SUFFIX}`)) {
    delegateAbsentScans.delete(row.id)
    return row
  }
  if (cancelledJobs.delete(row.id)) {
    delegateAbsentScans.delete(row.id)
    return { ...row, word: 'cancelled', endedAt: now }
  }
  const ledger = `${row.id}.json.delivered`
  if (names.has(ledger)) {
    delegateAbsentScans.delete(row.id)
    // The record is gone, so the ledger's own stop marker is all that is left
    // of the stop; a ledger an older build wrote has none and reads as a
    // plain finish.
    const delivered = await readJsonObject($, `${dir}/${ledger}`)
    const at = epochMsOf(delivered?.at)
    return { ...row, word: delivered?.stopped === true ? 'stopped' : 'done', endedAt: at === 0 ? now : at }
  }
  const absent = (delegateAbsentScans.get(row.id) ?? 0) + 1
  if (absent < DELEGATE_ABSENT_SCANS) {
    delegateAbsentScans.set(row.id, absent)
    return row
  }
  delegateAbsentScans.delete(row.id)
  return { ...row, word: row.word === 'blocking' ? 'done' : 'ended', endedAt: now }
}

// Whether a live record's owner process is gone. clauth asks the owner's own
// flock marker; a plugin cannot open it, so this asks the process instead,
// and only a clean `ps` answer decides: a pid this process can see — a reused
// one included — reads alive, and an unreadable or missing `ps` reads alive,
// so a failure never invents a dead owner. A `ps` that rejects the form fails
// exactly as an absent pid does, so the form is first asked about pid 1, a
// process every host has, and an answer that is the absence shape leaves every
// gone answer untrusted. A zombie has closed the marker's flock, so its stat
// counts as gone.
async function delegateOwnerGone($: EngineInterface, ownerPid: number): Promise<boolean> {
  try {
    if (psFormWorks === null) {
      const control = await $.process.run(['ps', '-o', 'stat=', '-p', '1'], {
        timeoutMs: DELEGATE_OWNER_PROBE_TIMEOUT_MS,
      })
      psFormWorks = !(control.exitCode === 1 && control.stdout.trim() === '')
    }
    if (!psFormWorks) return false
    const run = await $.process.run(['ps', '-o', 'stat=', '-p', String(ownerPid)], {
      timeoutMs: DELEGATE_OWNER_PROBE_TIMEOUT_MS,
    })
    if (run.exitCode === 1 && run.stdout.trim() === '') return true
    if (run.exitCode !== 0) return false
    const stat = run.stdout.trim()
    return stat !== '' && stat.startsWith('Z')
  } catch {
    return false
  }
}

// The child session a row's transcript file names, or null.
function transcriptSessionOf(transcript: string | null): string | null {
  if (transcript === null) return null
  const name = transcript.slice(transcript.lastIndexOf('/') + 1)
  return name.endsWith('.jsonl') ? name.slice(0, -'.jsonl'.length) : null
}

// One read of the job store: this session's records folded into the rows,
// a finished row kept as it last read, and one notice per row that left the
// live words. A store that cannot be listed changes nothing.
async function scanDelegates($: EngineInterface): Promise<void> {
  const epoch = sessionEpoch
  const dir = jobsDir
  if (dir === null || sessionEnding) return
  let entries
  try {
    entries = await $.fs.list(dir)
  } catch {
    return
  }
  const files = entries.filter(entry => entry.kind === 'file')
  const names = new Set(files.map(file => file.name))
  for (const name of jobFileStamps.keys()) if (!names.has(name)) jobFileStamps.delete(name)
  const sid = await $.session.id()
  const now = await $.clock.now()
  const before = await read($, DELEGATES)
  const rows = new Map(before.map(row => [row.id, row]))
  // A `/clear` took its session's rows with it; a run still live under it
  // comes back here until its record is read again.
  const carriedTaken = carriedRows
  for (const row of carriedTaken) if (!rows.has(row.id)) rows.set(row.id, row)
  const changed = await readChangedJobs($, dir, files, names)
  for (const { record, isBlocking } of changed) {
    const id = textOf(record.job_id)
    const kept = rows.get(id)
    if (kept === undefined) {
      const host = textOf(record.host_session)
      // This session's runs, and a run this process minted before a `/clear`
      // ended its session: the rows died with it, the record did not.
      const priorRun = textOf(record.state) === 'running' && priorSessions.has(host)
      if (host !== sid && !priorRun) continue
    }
    const row = delegateRowOf(record, isBlocking, now)
    // A finished row keeps its word, except one the owner probe marked
    // `orphaned`: the sweep's stopped tombstone that replaces its record is
    // the better word for the same run.
    if (kept !== undefined && !isLive(kept.word) && !(kept.word === 'orphaned' && row.word === 'stopped')) continue
    // A record first seen already finished before this session began was
    // noticed in the conversation that ran it: list it, never toast it again.
    rows.set(
      id,
      kept === undefined
        ? { ...row, isNoticed: !isLive(row.word) && row.endedAt < deliveredSince }
        : { ...row, isNoticed: kept.isNoticed },
    )
    recordWatches.set(id, {
      movedAt: now,
      ownerPid: typeof record.owner_pid === 'number' ? record.owner_pid : 0,
      probedAt: 0,
      silentAt: Math.max(
        epochMsOf(record.last_output_at),
        epochMsOf(record.recorded_at),
        epochMsOf(record.started_at),
      ),
    })
  }
  for (const id of recordWatches.keys()) {
    if (!names.has(`${id}.json`) && !names.has(`${id}${LIVE_SUFFIX}`)) recordWatches.delete(id)
  }
  for (const row of rows.values()) {
    if (isLive(row.word)) rows.set(row.id, await settleGoneDelegate($, dir, row, names, now))
    else if (row.word === 'orphaned' && !names.has(`${row.id}.json`) && names.has(`${row.id}.json.delivered`)) {
      // A stop collected before any read saw its tombstone leaves only the
      // ledger's stop marker to move an `orphaned` row to `stopped`.
      const delivered = await readJsonObject($, `${dir}/${row.id}.json.delivered`)
      if (delivered?.stopped === true) rows.set(row.id, { ...row, word: 'stopped' })
    }
  }
  // The owner's liveness is the one question the record's fields cannot
  // answer and a stale stamp answers wrong (a silent run is static too), so
  // it is asked of the process itself, only after the file has sat still and
  // once a minute per row. A record with no owner pid can only answer silence.
  for (const row of rows.values()) {
    if (!isLive(row.word)) continue
    const watch = recordWatches.get(row.id)
    if (watch === undefined) continue
    if (watch.ownerPid === 0) {
      if (now - watch.silentAt > DELEGATE_OWNERLESS_SILENT_MS) {
        rows.set(row.id, { ...row, word: 'orphaned', endedAt: now })
      }
      continue
    }
    if (now - watch.movedAt < DELEGATE_OWNER_STILL_MS) continue
    if (watch.probedAt !== 0 && now - watch.probedAt < DELEGATE_OWNER_PROBE_MS) continue
    watch.probedAt = now
    if (await delegateOwnerGone($, watch.ownerPid)) {
      rows.set(row.id, { ...row, word: 'orphaned', endedAt: now })
    }
  }
  const finished = [...rows.values()].filter(row => !isLive(row.word) && !row.isNoticed)
  for (const row of finished) rows.set(row.id, { ...row, isNoticed: true })
  const after = [...rows.values()].sort((a, b) => b.startedAt - a.startedAt)
  const live = after.some(row => isLive(row.word))
  // A read that started under a conversation that has since ended writes
  // nothing into the next one: a `/clear` or a `/resume` empties `$.state` and
  // moves the epoch, so this scan's rows, stamps, open and notices all belong
  // to a conversation that is gone. The next scan redoes the work under the
  // new id.
  if (epoch !== sessionEpoch) return
  // A read that adopts a live run takes over the ticks: a job minted outside
  // a delegate call has no call to start them.
  if (live && delegatesTimer === null && delegateCallsInFlight === 0) startDelegatesTimer($)
  const commitStamps = (): void => {
    for (const read of changed) jobFileStamps.set(read.file, read.stamp)
    carriedRows = carriedRows.filter(row => !carriedTaken.includes(row))
  }
  if (JSON.stringify(after) === JSON.stringify(before)) {
    commitStamps()
    return
  }
  // Stored before the notices go out: a write that fails leaves them owed to
  // the next tick, never raised twice. `update` retries its function, so it
  // reads the epoch again: a reset landing between its read and its set leaves
  // the state as it stood.
  await update($, DELEGATES, cur => (epoch !== sessionEpoch ? cur : after))
  if (epoch !== sessionEpoch) return
  // Committed only once the rows are stored: a write that failed leaves every
  // record read this scan owed to the next one.
  commitStamps()
  // Raised before the open: an unasked open that fails must not drop this
  // scan's notices, whose rows are already stored as told.
  for (const row of finished) {
    const elapsed = elapsedOf(row, now)
    $.ui.toast(`delegate ${row.id} on ${row.profile} ${row.word}${elapsed === null ? '' : ` after ${elapsed}`}`)
  }
  if (live) await autoOpenDelegates($, epoch)
}

// The store's one read in flight: a tick and a delegate call share it.
function scanDelegatesOnce($: EngineInterface): Promise<void> {
  if (delegateScanInFlight) return delegateScanInFlight
  delegateScanInFlight = scanDelegates($)
    .catch(() => {})
    .finally(() => {
      delegateScanInFlight = null
    })
  return delegateScanInFlight
}

// A read that starts after this call: one already in flight may have listed
// the store before the record a call just wrote.
async function scanDelegatesFresh($: EngineInterface): Promise<void> {
  await delegateScanInFlight
  await scanDelegatesOnce($)
}

function startDelegatesTimer($: EngineInterface): void {
  if (delegatesTimer !== null) return
  delegatesTimer = $.clock.every(DELEGATES_TICK_MS, () => delegatesTick($))
}

function stopDelegatesTimer(): void {
  delegatesTimer?.cancel()
  delegatesTimer = null
}

// One tick: the store read, the viewed transcript read where it grew, and
// the live rows' time redrawn. With nothing live and no call that could
// start a run, the ticks stop until the next `delegate` call.
async function delegatesTick($: EngineInterface): Promise<void> {
  try {
    const epoch = sessionEpoch
    await scanDelegatesOnce($)
    // A conversation that ended under this scan owns the ticks now: a `/clear`
    // keeps them running for the runs it carried, and a `/resume` has stopped
    // them. Reading the emptied state here would stop the `/clear`'s ticks
    // before the next scan can adopt its runs.
    if (epoch !== sessionEpoch) return
    const rows = await read($, DELEGATES)
    await refreshDelegateView($, rows)
    if (rows.some(row => isLive(row.word))) $.ui.invalidate('ui.render')
    else if (delegateCallsInFlight === 0) stopDelegatesTimer()
  } catch {
    // A failed tick changes nothing; the next one reads again.
  }
}

const oneLine = (s: string): string => {
  const flat = s.replace(/\s+/g, ' ').trim()
  return flat.length > TRANSCRIPT_LINE_CHARS ? `${flat.slice(0, TRANSCRIPT_LINE_CHARS - 1)}…` : flat
}

// One content block as a line: text as written, a tool call as its name and
// input; every other block (a tool's result, thinking) is left out.
function blockLine(block: unknown, lead: string): string[] {
  const fields = objectOf(block)
  if (fields === null) return []
  if (fields.type === 'text') {
    const said = oneLine(textOf(fields.text))
    return said === '' ? [] : [`${lead}${said}`]
  }
  if (fields.type === 'tool_use') return [oneLine(`⚙ ${textOf(fields.name)} ${oneLineJson(fields.input ?? {})}`)]
  return []
}

// One transcript entry as the lines the view shows: the prompts a delegate
// was given (`›`), its replies and its tool calls.
function entryLines(entry: JsonObject): string[] {
  const type = entry.type
  if (type !== 'user' && type !== 'assistant') return []
  const lead = type === 'user' ? '› ' : ''
  const content = objectOf(entry.message)?.content
  if (typeof content === 'string') return blockLine({ type: 'text', text: content }, lead)
  if (!Array.isArray(content)) return []
  return content.flatMap(block => blockLine(block, lead))
}

// A transcript's end as lines; the first entry, cut by the byte bound, does
// not parse and is left out.
function transcriptLines(jsonl: string): string[] {
  const lines: string[] = []
  for (const raw of jsonl.split('\n')) {
    let entry: JsonObject | null
    try {
      entry = objectOf(JSON.parse(raw))
    } catch {
      continue
    }
    if (entry !== null) lines.push(...entryLines(entry))
  }
  return lines.slice(-TRANSCRIPT_LINES)
}

async function readTranscript($: EngineInterface, path: string): Promise<string[]> {
  try {
    const run = await $.process.run(['tail', '-c', String(TRANSCRIPT_TAIL_BYTES), path], {
      timeoutMs: TRANSCRIPT_READ_TIMEOUT_MS,
    })
    if (run.exitCode !== 0) return [NO_TRANSCRIPT]
    const lines = transcriptLines(run.stdout)
    return lines.length === 0 ? [EMPTY_TRANSCRIPT] : lines
  } catch {
    return [NO_TRANSCRIPT]
  }
}

// The view's transcript, read again only when its file changed since. A pane
// the engine dropped as `unload` (its drawing threw) runs no closer hook, so
// the view can outlive it: nothing draws it, so nothing here reads its file.
async function refreshDelegateView($: EngineInterface, rows: readonly ClauthDelegateRow[]): Promise<void> {
  if (!(await $.ui.panes()).some(pane => pane.id === DELEGATES_PANE)) return
  const view = await read($, DELEGATE_VIEW)
  if (view === null) return
  const path = rows.find(row => row.id === view.id)?.transcript ?? null
  if (path === null) return
  const stamp = await fileStamp($, path)
  if (stamp === null || stamp === viewStamp) return
  const lines = await readTranscript($, path)
  viewStamp = stamp
  await update($, DELEGATE_VIEW, cur => (cur?.id === view.id ? { id: view.id, lines } : cur))
}

async function openDelegateView($: EngineInterface, id: string): Promise<void> {
  const path = (await read($, DELEGATES)).find(row => row.id === id)?.transcript ?? null
  viewStamp = path === null ? null : await fileStamp($, path)
  const lines = path === null ? [NO_TRANSCRIPT] : await readTranscript($, path)
  await update($, DELEGATE_VIEW, () => ({ id, lines }))
}

// The cancel a blocking delegate call settled on: its envelope's word, and
// the child session id its prose names. A settle that is not a cancelled
// envelope answers null.
function cancelledDelegateOf(settled: ToolCallResult): { session: string | null } | null {
  if (settled.deny !== undefined) return null
  const text = settled.text ?? ''
  if (!/^delegate to `[^`]*` cancelled(?: after \d+s)?:/.test(text)) return null
  return { session: /resume with session id `([^`]+)`/.exec(text)?.[1] ?? null }
}

// Marks the blocking row a cancelled call settled: the settle's child session
// id names its transcript, and a record read before that id arrived leaves
// the session's one live blocking row to settle. A settle with no session id
// is a pre-spawn cancel, which names no run, so no row may take its word. The
// row settles on the scan that finds its record gone.
async function noteCancelledDelegate($: EngineInterface, settled: ToolCallResult): Promise<void> {
  const cancelled = cancelledDelegateOf(settled)
  if (cancelled === null || cancelled.session === null) return
  const live = (await read($, DELEGATES)).filter(row => row.word === 'blocking')
  const target =
    live.find(row => transcriptSessionOf(row.transcript) === cancelled.session) ??
    (live.length === 1 ? live[0] : undefined)
  if (target !== undefined) cancelledJobs.add(target.id)
}

// clauth's cancel reply opens with what the ask did: `asked <id> to stop`
// only when this server held a run under that id and set its flag. Every
// other clause (`already finished`, a foreign owner, ...) means nothing here
// was stopping, so the record keeps the last word.
const CANCEL_ASKED = /^asked `([^`]+)` to stop/

// Asks clauth's `monitor` to cancel one run: first on the server its tool
// name implies, then on the `/mcp` spellings. A run the reply names as asked
// to stop is marked cancelled here, because its record can go before the next
// scan reads it and only the reply carries the cancel's word; otherwise the
// row turns over with its record.
//
// A run whose record already finished is left to the next scan: a cancel
// claims a done record and delivers its result to this call, which the mod
// drops, taking the delegate's work from the model. That covers a blocking
// run Esc handed off to the background, whose `<id>.json` the row's last word
// cannot tell; a blocking run still under its `.live.json` has no readable
// record here, so the call goes ahead.
async function stopDelegate($: EngineInterface, row: ClauthDelegateRow): Promise<void> {
  const id = row.id
  const dir = jobsDir
  if (dir !== null) {
    const record = await readJsonObject($, `${dir}/${id}.json`)
    if (record !== null && textOf(record.state) !== 'running') return
  }
  const servers = new Set<string>([
    ...(monitorTool === null ? [] : [TOOL_SERVERS[monitorTool]]),
    monitorServer,
    ...Object.values(MONITOR_SERVERS),
  ])
  for (const server of servers) {
    try {
      const result = await $.mcp.call(server, 'monitor', { job_ids: [id], cancel: true })
      // clauth prepends the ask's note even to an error reply: a blocking run
      // has no collectable record, so its "still running" refusal still says
      // the run was asked to stop.
      const reply = result.content.map(block => (typeof block.text === 'string' ? block.text : '')).join('')
      if (CANCEL_ASKED.exec(reply)?.[1] === id) {
        cancelledJobs.add(id)
        return
      }
      if (result.isError !== true) return
      break
    } catch {
      // Not connected under this name: the next spelling is tried.
    }
  }
  $.ui.toast(`could not stop ${id}`)
}

function openDelegatesPane($: EngineInterface) {
  return $.ui.open({ id: DELEGATES_PANE, title: DELEGATES_TITLE })
}

// Whether an unasked open may land here: only where the pane is a sidebar on
// every surface the session draws on. The API's own example opens only on
// `isFullscreen === true`, and a surface that has not said, or a session that
// draws on none, is not a yes.
const docksPane = async ($: EngineInterface): Promise<boolean> => {
  const surfaces = await $.session.surfaces()
  return surfaces.length > 0 && surfaces.every(surface => surfaceLayouts.get(surface) === true)
}

// The one unasked open a delegate call gets: the scan that sees its run opens
// the pane while the call holds the turn, so a blocking run can be watched and
// stopped, and a call no scan caught still opens it once it settles. A call
// that finds the pane already open spends its open, and a close during the
// call is the person's, not an invitation to reopen it at the settle. A scan
// whose conversation has ended since it started opens nothing.
async function autoOpenDelegates($: EngineInterface, epoch: number | null = null): Promise<void> {
  const stale = (): boolean => epoch !== null && epoch !== sessionEpoch
  if (stale() || delegateCallsInFlight === 0 || autoOpenGen === delegateCallGen) return
  if (!(await docksPane($))) return
  if (stale()) return
  autoOpenGen = delegateCallGen
  if ((await $.ui.panes()).some(pane => pane.id === DELEGATES_PANE)) return
  if (stale()) return
  await openDelegatesPane($)
}

// After a delegate call: the store read fresh, then the pane opened unless
// this session has no delegate to list.
async function showDelegates($: EngineInterface): Promise<void> {
  await scanDelegatesFresh($)
  if ((await read($, DELEGATES)).length === 0) return
  await autoOpenDelegates($)
}

// The hint line's control: the live count while a run goes, as the engine's
// own pills count theirs, else the bare name, so it stays once all finished.
function delegatesControlLabel(rows: readonly ClauthDelegateRow[]): string {
  const live = rows.filter(row => isLive(row.word)).length
  if (live === 0) return 'delegates'
  return live === 1 ? '1 delegate' : `${live} delegates`
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
    // A run that finished before this session started was noticed in the
    // conversation that ran it, so its record is listed and never toasted
    // again. Read before the job store's path below opens it to a store read:
    // a draw that schedules one must find the bound already set.
    deliveredSince = await $.clock.now()
    socketPath = `${home}/.clauth/bridge.sock`
    jobsDir = `${home}/.clauth/jobs`
    projectsDir = `${home}/.claude/projects`
    try {
      await $.command.register({
        name: DELEGATES_COMMAND,
        description: "open the pane listing this session's clauth delegates",
      })
    } catch {
      // A refused name leaves the pane reachable from a delegate call and the
      // hint line; the bridge below must still start.
    }
    // The ids a `/clear` ended and the rows it carried belong to the
    // conversation that ended; a session that starts fresh owns neither. No
    // engine path reaches this today (`session.start` fires once per process,
    // and a reload's module state is fresh), so it guards the invariant a
    // future path must keep.
    priorSessions.clear()
    carriedRows = []
    // A reload keeps the rows but not the timer: a live one needs it back.
    if ((await read($, DELEGATES)).some(row => isLive(row.word))) startDelegatesTimer($)
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
    // Raised before anything awaits: a store read already in flight must see
    // the conversation gone before it can write its rows into the next one.
    sessionEpoch += 1
    // A store read started from inside the end would carry the new epoch and
    // the old conversation's rows: none starts while it runs.
    sessionEnding = true
    try {
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
      // A `/clear` starts a new session id with no `session.start`, so the rows
      // kept in `$.state` go with it; a run still live under the old id is
      // adopted again from its record, and the ticks keep reading for it.
      if (e.reason === 'clear') {
        if (e.sessionId !== '') priorSessions.add(e.sessionId)
        // The engine drops the rows with the conversation, and the records do
        // not move: the live ones are kept here and every record is read again,
        // or a run whose record was already read and stays still is lost.
        carriedRows = (await read($, DELEGATES)).filter(row => isLive(row.word))
        jobFileStamps.clear()
        startDelegatesTimer($)
      } else {
        // A `/resume` is another conversation with no `session.start`: the ids
        // an earlier `/clear` ended, the rows it carried and the stamps all
        // belong to the conversation this one left.
        if (processContinues) {
          priorSessions.clear()
          carriedRows = []
          jobFileStamps.clear()
        }
        stopDelegatesTimer()
      }
      if (!inert) {
        if (!processContinues) stopPollLoop()
        // Await the `end` post alone (the engine's own short bound caps it), so
        // the daemon learns the end now rather than from its silence sweep.
        await postEvent($, 'end', {}, ended)
      }
      return await next(e)
    } finally {
      // Bumped again so a callback that ran inside the end cannot act on the
      // conversation after it once the end is over.
      sessionEpoch += 1
      sessionEnding = false
    }
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
    const settled = await next(e).finally(() => closeUnheldAsks($, e.tool_use_id))
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
      if (!opened.held) {
        closeUnheldAskWhenSettled($, ask, opened.session, e.tool_use_id)
        return v
      }
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
      if (outcome.type === 'released') {
        // The desk's settle of the call closes it, after the engine's dialog
        // and the call's run; a bare query has no call and closes here.
        closeUnheldAskWhenSettled($, ask, opened.session, e.tool_use_id)
        return v
      }
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
      if (!opened.held) {
        // The engine's own question dialog runs inside `next`.
        return next(e).finally(() => closeUnheldAsk($, ask, opened.session))
      }
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
      if (outcome.type === 'released') {
        // Released to the desk: the engine's own question dialog runs inside
        // `next`, and the ask closes once it returns, as an unheld one does.
        return next(e).finally(() => closeUnheldAsk($, ask, opened.session))
      }
      return next(e)
    },
  ).catch(($, e, next) => next(e))

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    noteLayout(e.surface, e.viewport)
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

  // A delegate call starts the store's ticks before it runs, so the run shows
  // while its call holds the turn: the scan that sees it opens the pane, and
  // a call whose run no scan caught still opens it once it settles.
  on('tool.call', { tool: DELEGATE_TOOLS }, async ($, e, next) => {
    if (inert) return next(e)
    monitorTool = e.tool as DelegateTool
    monitorServer = MONITOR_SERVERS[monitorTool]
    delegateCallsInFlight += 1
    delegateCallGen += 1
    startDelegatesTimer($)
    try {
      // A pane the person already holds needs no unasked open, and a close
      // during this call is theirs to keep.
      if ((await $.ui.panes().catch(() => [])).some(pane => pane.id === DELEGATES_PANE)) autoOpenGen = delegateCallGen
      const settled = await next(e)
      // A cancel is the call's own settle, not the record: the liveness file
      // is gone before the envelope comes back, so this must be read here.
      await noteCancelledDelegate($, settled)
      if (settled.deny === undefined && settled.isError !== true) await showDelegates($)
      return settled
    } finally {
      delegateCallsInFlight -= 1
    }
  }).catch(($, e, next) => next(e))

  // The person asked, so the pane opens at any width and with no delegate
  // yet: its empty list says there is none.
  on('command.run', { command: DELEGATES_COMMAND }, async ($, e) => {
    noteLayout('terminal', e.presentation)
    // The ticks run only while a row is live, so a job minted outside this
    // module's own delegate calls is invisible until a read: ask for one now.
    await scanDelegatesFresh($)
    const opened = await openDelegatesPane($)
    return { text: opened.isPlaced ? 'delegates pane open' : `delegates pane waits: ${opened.reason}` }
  }).catch(() => ({ text: 'could not open the delegates pane' }))

  // The engine's line stays its own node on its own row, so its pills keep
  // counting; the control sits on the row beneath it once the session has had
  // a delegate. Only the fullscreen terminal reports clicks, so on the main
  // screen the control could never be pressed.
  on('ui.render', { component: 'PromptHint' }, async ($, e, next) => {
    noteLayout(e.surface, e.viewport)
    // A drawing may not write state, so the read runs as its own event on the
    // clock: a job minted outside a delegate call lands in the store, and its
    // write redraws this line with the control on it. The ticks already read
    // while a row is live; with them stopped, one draw a second asks.
    const now = await $.clock.now()
    if (delegatesTimer === null && now - hintScanAt >= HINT_SCAN_MS) {
      hintScanAt = now
      $.clock.after(0, () => void scanDelegatesOnce($))
    }
    const rows = await read($, DELEGATES)
    if (rows.length === 0 || e.viewport?.isFullscreen === false) return next(e)
    const { Box, Text, Button } = $.ui.resolve(e)
    const engine = await next(e)
    return Box({
      flexDirection: 'column',
      children: [
        engine,
        Box({
          flexDirection: 'row',
          children: [
            Text({ dimColor: true, children: ' · ' }),
            Button({
              key: 'delegates',
              plain: true,
              dimColor: true,
              label: delegatesControlLabel(rows),
              onPress: () => void openDelegatesPane($),
            }),
          ],
        }),
      ],
    })
  })

  // A closed pane drops the transcript view with it: the refresh stops
  // reading a transcript nothing draws, and a reopen lands on the list.
  on('ui.close', { id: DELEGATES_PANE }, async ($, e, next) => {
    if ((await read($, DELEGATE_VIEW)) !== null) {
      viewStamp = null
      await update($, DELEGATE_VIEW, () => null)
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  on('ui.render', { component: 'Pane', requestId: DELEGATES_PANE }, async ($, e) => {
    noteLayout(e.surface, e.viewport)
    const { Box, Text, Button } = $.ui.resolve(e)
    const rows = await read($, DELEGATES)
    const view = await read($, DELEGATE_VIEW)
    const now = await $.clock.now()
    const viewed = view === null ? undefined : rows.find(row => row.id === view.id)
    const back = () => void update($, DELEGATE_VIEW, () => null)
    const stop = (row: ClauthDelegateRow) => () => void stopDelegate($, row)

    if (view !== null && viewed !== undefined) {
      const room = Math.max(1, e.props.scroll.bodyRows - 2)
      const controls = [Button({ key: 'back', plain: true, hotkey: 'b', label: 'back', onPress: back })]
      if (isLive(viewed.word)) {
        controls.push(Text({ children: '   ' }), Button({ key: 'stop', plain: true, hotkey: 's', label: 'stop', onPress: stop(viewed) }))
      }
      return Box({
        flexDirection: 'column',
        children: [
          Text({ wrap: 'truncate-end', children: delegateSummary(viewed, now) }),
          ...view.lines.slice(-room).map(line => Text({ dimColor: true, wrap: 'truncate-end', children: line })),
          Box({ children: controls }),
        ],
      })
    }

    if (rows.length === 0) return Text({ dimColor: true, children: 'no delegates in this session yet' })
    return Box({
      flexDirection: 'column',
      children: rows.map((row, i) => {
        const cells = [
          Button({
            key: `open-${row.id}`,
            plain: true,
            ...(i < 9 ? { hotkey: String(i + 1) } : {}),
            onPress: () => void openDelegateView($, row.id),
            children: delegateSummary(row, now),
          }),
        ]
        if (isLive(row.word)) {
          cells.push(Text({ children: '  ' }), Button({ key: `stop-${row.id}`, plain: true, label: 'stop', onPress: stop(row) }))
        }
        if (row.tail !== '') cells.push(Text({ dimColor: true, wrap: 'truncate-end', children: `  ${row.tail}` }))
        return Box({ key: `row-${row.id}`, children: cells })
      }),
    })
  })
}
