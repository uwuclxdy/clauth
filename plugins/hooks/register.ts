// One Claude Code session bridged to the clauth daemon over its same-user unix
// socket, so a paired phone can answer the session's permission asks and steer
// it without owning the desk's keystrokes.
import { update } from 'claude-code'
import type { EngineInterface, Register, Timer } from 'claude-code'

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

const SEQ = { plugin: 'clauth', key: 'seq' } as const
const HELD = { plugin: 'clauth', key: 'heldRow' } as const

const encoder = new TextEncoder()

type HoldOutcome =
  | { type: 'allow' | 'deny'; from: 'desk' | 'device' }
  | { type: 'answers'; answers: Record<string, string> }
  | { type: 'loss' }

type HeldEntry = {
  kind: 'decision' | 'answers'
  tool: string
  desc: string
  turn: string | null
  resolve: (o: HoldOutcome) => void
}

// Per-load runtime state. The only value a hot reload must keep, the event
// sequence number, lives in `$.state` (SEQ); everything here is re-established
// by the `session.start` that fires again on a reload.
let inert = false
let socketPath: string | null = null
let sessionId: string | null = null
let interactive = false
let pollStarted = false
let pollCancel: Timer | null = null
let pollInFlight: Promise<void> | null = null
let sendChain: Promise<unknown> = Promise.resolve()
let nextPollAfter = 0
let turnId: string | null = null
let pendingPrompt: { text: string } | null = null
let helds = new Map<string, HeldEntry>()
let interruptedTurn: string | null = null
let askCounter = 0
// The last cadence run per scope, keyed by session and agent id.
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

async function postEvent(
  $: EngineInterface,
  kind: string,
  fields: Record<string, unknown> = {},
): Promise<void> {
  if (inert || sessionId === null) return
  await enqueueSend(async () => {
    const seq = await nextSeq($)
    const body = JSON.stringify({ session: sessionId, seq, kind, ...fields })
    if (bodyBytes(body) > BODY_LIMIT) return
    try {
      await fetchBridge($, '/event', 'POST', body)
    } catch {
      // A refused or unreachable daemon must not stall the session: the event
      // is best-effort, the daemon re-syncs from its own state.
    }
  }).catch(() => {})
}

// Posts `ask_opened` and reads the daemon's hold answer; null means the ask
// must not be held (over-bound body, unreachable socket, or a non-ok reply).
async function postAskOpened(
  $: EngineInterface,
  ask: string,
  tool: string,
  input: unknown,
  agentId: string | undefined,
): Promise<boolean | null> {
  if (sessionId === null) return null
  return enqueueSend(async () => {
    const seq = await nextSeq($)
    const fields: Record<string, unknown> = { ask, tool, input }
    if (agentId !== undefined) fields.agent_id = agentId
    const body = JSON.stringify({ session: sessionId, seq, kind: 'ask_opened', ...fields })
    if (bodyBytes(body) > BODY_LIMIT) return null
    try {
      const res = await fetchBridge($, '/event', 'POST', body)
      if (!res.ok) return null
      const parsed = JSON.parse(res.text) as { hold?: boolean }
      return parsed.hold === true
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
    await postEvent($, 'ask_closed', { ask, by })
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
  const answers: Record<string, string> = {}
  for (const [k, v] of Object.entries(cmd.answers as Record<string, unknown>)) {
    if (typeof v !== 'string') return
    answers[k] = v
  }
  if (Object.keys(answers).length === 0) return
  await answerFromDevice($, ask, { type: 'answers', answers })
}

async function handleCommand($: EngineInterface, cmd: Record<string, unknown>): Promise<void> {
  if (typeof cmd !== 'object' || cmd === null) return
  switch (cmd.kind) {
    case 'prompt':
      if (typeof cmd.text === 'string') {
        if (turnId !== null) {
          pendingPrompt = { text: cmd.text }
        } else {
          try {
            await $.prompt.submit({ text: cmd.text })
          } catch {
            // A refused submit while idle must not drop the prompt: the device
            // already got 200, so keep it for the next turn.complete.
            pendingPrompt = { text: cmd.text }
          }
        }
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

async function onPollError($: EngineInterface): Promise<void> {
  const now = await $.clock.now()
  nextPollAfter = now + POLL_BACKOFF_MS
}

// One timer poll: the session's only poll, carrying prompts, interrupts and
// (redundantly) answers. A held check never pumps this loop — it waits on its
// own `/wait` — so a poll error or a dropped remote flag here touches no hold.
async function doPoll($: EngineInterface): Promise<void> {
  try {
    let res
    try {
      res = await fetchBridge($, `/poll?session=${encodeURIComponent(sessionId as string)}`, 'GET')
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

// The timer's single in-flight poll: at most one timer poll parks at a time.
function pollOnce($: EngineInterface): Promise<void> {
  if (pollInFlight) return pollInFlight
  pollInFlight = doPoll($).finally(() => {
    pollInFlight = null
  })
  return pollInFlight
}

// One wait on one ask: a single `/wait` fetch, parked at the daemon until that
// ask settles. The awaited fetch pauses the hook budget, so a hold outlives the
// 10 s bound. Every non-settle reads as a loss for this ask alone.
async function waitOnce($: EngineInterface, ask: string): Promise<void> {
  let res
  try {
    res = await fetchBridge(
      $,
      `/wait?session=${encodeURIComponent(sessionId as string)}&ask=${encodeURIComponent(ask)}`,
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
      // The answer's form did not match this hold (a decision for a held
      // question, a non-string or empty `answers`, ...): the daemon already
      // closed the ask, so resolve it as a loss rather than re-waiting on a
      // closed ask forever.
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

// Holds `ask` until it resolves (desk, phone, loss, or an abort), waiting on
// that ask's own `/wait` so the hook budget stays paused on an awaited fetch.
// The race against the outcome promise makes a desk press or an abort end the
// wait without waiting out the parked fetch; its later reply is then ignored.
async function holdUntilSettled(
  $: EngineInterface,
  ask: string,
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
    await Promise.race([hp, waitOnce($, ask)])
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
  transcript_path: string
  tool_name: string
  agent_id?: string
}

// The window one fire belongs to: a subagent's calls never spend the main
// thread's window or another subagent's.
function noteWindowKey(e: { session_id: string; agent_id?: string }): string {
  return `${e.session_id}/${e.agent_id ?? ''}`
}

// The classic hook input the note command reads, taken from the event the
// engine raised: `clauth`'s own parser keeps these fields, and the scope is
// the event's, so a subagent's note lands on the subagent's record.
function noteInput(e: NoteScope): string {
  return JSON.stringify({
    session_id: e.session_id,
    transcript_path: e.transcript_path,
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
    await $.state.set(HELD, null)
    void postEvent($, 'start', { cwd: e.cwd, interactive: e.isInteractive })
    startPollLoop($)
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    if (!inert) {
      stopPollLoop()
      // Await the `end` post alone (the engine's own short bound caps it), so
      // the daemon learns the end now rather than from its silence sweep.
      await postEvent($, 'end')
    }
    return next(e)
  })

  on('session.append', async ($, e, next) => {
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
    if (!inert && e.agentId === undefined) {
      turnId = null
      interruptedTurn = null
      void postEvent($, 'turn_end', { turn: e.turnId })
      if (pendingPrompt) {
        const p = pendingPrompt
        try {
          await $.prompt.submit(p)
          pendingPrompt = null
        } catch {
          // The submit was refused; keep the prompt for the next turn.complete.
        }
      }
    }
    return next(e)
  })

  on('classic.PostToolUse', async ($, e, next) => {
    // A main-scope agent-spawn call is what the headroom nudge is computed
    // on, and a skipped fire never replays it, so it runs the command
    // whatever the window says and leaves the window where it stood.
    const spawn = e.agent_id === undefined && AGENT_SPAWN_TOOLS.includes(e.tool_name)
    if (!spawn) {
      const key = noteWindowKey(e)
      const now = await $.clock.now()
      const lastAt = lastNoteAt.get(key)
      if (lastAt !== undefined && now - lastAt < NOTE_CADENCE_MS) return next(e)
      // A scope that never fires its stop ages out here, so the map holds
      // only the scopes that ran inside one window.
      for (const [stale, at] of lastNoteAt) {
        if (now - at >= NOTE_CADENCE_MS) lastNoteAt.delete(stale)
      }
      // Stamped before the run's own awaits, so tools settling together share
      // the window instead of each spawning.
      lastNoteAt.set(key, now)
    }
    const settled = await next(e)
    const note = await runNote($, e)
    if (note === null) return settled
    return { ...settled, additionalContext: [...(settled.additionalContext ?? []), note] }
  }).catch(($, e, next) => next(e))

  on('classic.SubagentStop', async ($, e, next) => {
    lastNoteAt.delete(noteWindowKey(e))
    return next(e)
  }).catch(($, e, next) => next(e))

  on(
    'tool.check',
    async ($, e, next) => {
      if (inert) return next(e)
      const v = await next(e)
      if (v.decision !== 'ask') return v
      if (!interactive) return v
      const ask = mintAskId()
      const turn = turnId
      const heldNow = await postAskOpened($, ask, e.tool, e.input, e.agentId)
      if (heldNow !== true) return v
      const outcome = await holdUntilSettled(
        $,
        ask,
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
      const heldNow = await postAskOpened($, ask, 'AskUserQuestion', e.questions, undefined)
      if (heldNow !== true) return next(e)
      const outcome = await holdUntilSettled(
        $,
        ask,
        'answers',
        'AskUserQuestion',
        firstQuestion(e.questions),
        turn,
        next.signal,
      )
      if (outcome.type === 'answers') {
        return { result: { answers: outcome.answers } }
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
              Text({ children: '   Esc denies' }),
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
