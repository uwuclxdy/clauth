import { test, expect, mock } from 'claude-code/testing'
import type { Plugin } from 'claude-code/testing'
import type { HttpInit, On, ToolCallResult } from 'claude-code'

// The test worker has the runtime's timers but not their declarations in the
// type file; the budget and race tests await real time through it.
declare function setTimeout(callback: () => void, ms: number): unknown

type FetchRecord = { url: string; init?: HttpInit }
type EventBody = Record<string, unknown>

// How long the bridge parks a poll or a wait with nothing to deliver, in
// mock-clock time; it stands for the daemon's up-to-20 s park.
const PARK_MS = 1000

function res(status: number, text: string) {
  return { value: { status, ok: status >= 200 && status < 300, headers: {}, text } }
}

// The `ask` query value of a `GET /wait?session=...&ask=...` URL.
function parseWaitAsk(url: string): string {
  const query = url.split('?')[1] ?? ''
  for (const pair of query.split('&')) {
    const [key, value] = pair.split('=')
    if (key === 'ask') return value ?? ''
  }
  return ''
}

function installBridge(on: On, clock: { sleep: (ms: number) => Promise<void> }, askHold = true) {
  const fetches: FetchRecord[] = []
  const events: EventBody[] = []
  let lastAsk: string | null = null
  // Ask id -> its device answer (null while open) and closed flag, standing for
  // the daemon's closed-asks record. A held check waits on its own `/wait`,
  // which parks until this record says the ask settled.
  const asks = new Map<string, { answer: Record<string, unknown> | null; closed: boolean }>()
  const poll = {
    commands: [] as unknown[],
    remote: true,
    fail: null as null | 'error' | 'reject',
  }
  const wait = {
    remote: true,
    fail: null as null | 'error' | 'reject',
  }
  // A `start` naming this id answers 403 `session_mismatch`, as the daemon
  // does while the pid's session record still names another id.
  const start = { refuse: null as string | null }
  // An `ask_opened` the daemon refuses (a 403/409 the event route answers
  // before it records the ask).
  const opened = { refuse: false }
  // A device answer: the daemon closes the ask and keeps the answer for its
  // wait. An ask already closed refuses (the device got `ask_closed`).
  const answer = (ask: string, payload: Record<string, unknown>): void => {
    const st = asks.get(ask)
    if (!st || st.closed) return
    st.answer = payload
    st.closed = true
  }
  on('http.fetch', ($, e, next) => {
    fetches.push({ url: e.url, init: e.init })
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined) events.push(body)
    if (e.url.includes('/wait')) {
      if (wait.fail === 'reject') {
        wait.fail = null
        throw new Error('bridge down')
      }
      if (wait.fail === 'error') return res(500, 'boom')
      const ask = parseWaitAsk(e.url)
      const deliver = () => {
        const st = asks.get(ask)
        if (st === undefined) return res(404, JSON.stringify({ error: 'ask_not_found' }))
        if (st.answer !== null) {
          return res(200, JSON.stringify({ remote: wait.remote, answer: st.answer, closed: true }))
        }
        if (st.closed) return res(200, JSON.stringify({ remote: wait.remote, answer: null, closed: true }))
        return res(200, JSON.stringify({ remote: wait.remote, answer: null, closed: false }))
      }
      // A closed ask answers at once, as the daemon does: an already-settled
      // ask never parks another wait.
      const st = asks.get(ask)
      if (st !== undefined && (st.answer !== null || st.closed)) return deliver()
      return clock.sleep(PARK_MS).then(deliver)
    }
    if (e.url.includes('/poll')) {
      if (poll.fail === 'reject') {
        poll.fail = null
        throw new Error('bridge down')
      }
      if (poll.fail === 'error') return res(500, 'boom')
      const deliver = () => {
        const commands = poll.commands
        poll.commands = []
        return res(200, JSON.stringify({ remote: poll.remote, commands }))
      }
      if (poll.commands.length > 0) return deliver()
      return clock.sleep(PARK_MS).then(deliver)
    }
    if (body !== undefined && body.kind === 'start' && body.session === start.refuse) {
      return res(403, JSON.stringify({ error: 'session_mismatch' }))
    }
    if (body !== undefined && body.kind === 'ask_opened' && opened.refuse) {
      return res(409, JSON.stringify({ error: 'stale_sequence' }))
    }
    if (body !== undefined && body.kind === 'ask_opened') {
      lastAsk = body.ask as string
      asks.set(body.ask as string, { answer: null, closed: false })
      return res(200, JSON.stringify({ ok: true, remote: true, hold: askHold }))
    }
    if (body !== undefined && body.kind === 'ask_closed') {
      const st = asks.get(body.ask as string)
      if (st && !st.closed) st.closed = true
      return res(200, JSON.stringify({ ok: true }))
    }
    return res(200, JSON.stringify({ ok: true }))
  })
  return { fetches, events, lastAsk: () => lastAsk, poll, wait, start, opened, answer }
}

function installBoilerplate(
  on: On,
  env: Record<string, string> = { HOME: '/home/u' },
  session: { down: boolean; id?: string } = { down: false },
) {
  const clock = mock.clock(on)
  mock.env(on, env)
  // `id` moves the way a `/clear` moves the process's session id.
  on('session.id', ($, e, next) => {
    if (session.down) throw new Error('session down')
    return { value: session.id ?? 'sid-1' }
  })
  on('session.start', ($, e, next) => ({ cwd: e.cwd }))
  on('session.end', ($, e, next) => ({ sessionId: e.sessionId }))
  on('turn.start', ($, e, next) => ({ turnId: e.turnId }))
  on('turn.complete', ($, e, next) => ({ text: '' }))
  // The engine's own band, beneath the mod's: `$.ui.mount` needs something to
  // answer ui.render when the mod passes it on (nothing is held).
  on('ui.render', ($, e, next) => ({ type: 'Box' }))
  return { clock }
}

function setup(
  on: On,
  env: Record<string, string> = { HOME: '/home/u' },
  hold = true,
  session?: { down: boolean; id?: string },
) {
  const { clock } = installBoilerplate(on, env, session)
  const bridge = installBridge(on, clock, hold)
  return { clock, bridge }
}

// One whole main turn: its start, then its complete.
type TurnDriver = {
  turn: {
    start(e: { text: string; turnId: string }): Promise<unknown>
    complete(e: {
      reason: 'answer'
      answer: string
      durationMs: number
      isAborted: boolean
      turnId: string
    }): Promise<unknown>
  }
}

async function turn($: TurnDriver, turnId: string) {
  await $.turn.start({ text: 'x', turnId })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId })
}

// The prompt outcomes the mod reported, in post order, as the daemon reads them.
function promptReports(bridge: { events: EventBody[] }) {
  return bridge.events
    .filter(e => e.kind === 'prompt')
    .map(e => ({
      prompt: e.prompt,
      state: e.state,
      ...(e.reason !== undefined ? { reason: e.reason } : {}),
    }))
}

// The texts Claude Code 2.1.322 drops a prompt with before accepting it, while
// plugin input is held; a drop with any other value is final.
const HELD_DROPS = [
  'the session was stopped and nobody has sent input since',
  'the host has ended its input',
  'the run is past its --max-budget-usd limit',
  'the run reached its --max-turns limit',
]
const REMOVED_DROP = 'the prompt was removed from the queue before it ran'

function band(bodyColumns: number) {
  return {
    hasSurvey: false,
    isWorking: false,
    maxRows: 10,
    bodyColumns,
    scroll: { offset: 0, bodyRows: 5 },
    view: {},
  }
}

function appendRow(uuid: string, agentId?: string) {
  return {
    message: { type: 'user' as const, content: [{ type: 'text', text: 'hi' }] },
    door: 'prompt' as const,
    origin: { kind: 'composer' as const },
    uuid,
    ...(agentId !== undefined ? { agentId } : {}),
  }
}

// The cadence the mod runs the note command at, in mock-clock milliseconds:
// pinned to the shipped value, because the same run stamps the account the
// token-speed reader attributes spend to, so the window also bounds how long
// a switch stays attributed to the old account.
const NOTE_CADENCE_MS = 10_000

// What `clauth hook-profile-changed-note` prints for a fire that earned one or
// more notes: a single classic envelope, every note joined into one string.
function noteEnvelope(...notes: string[]): string {
  return JSON.stringify({
    hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: notes.join('\n\n') },
  })
}

type NoteRun = { argv: readonly string[]; init?: { stdin?: string; timeoutMs?: number } }

// The note subcommand as the test sees it: every run recorded, each answered
// from `answer` (the caller mutates it between windows) instead of spawning a
// real `clauth`.
function installNoteCommand(on: On, answer: { stdout?: string; exitCode?: number }): NoteRun[] {
  const runs: NoteRun[] = []
  on('process.run', ($, e, next) => {
    runs.push({ argv: e.argv, init: e.init })
    return {
      value: {
        exitCode: answer.exitCode ?? 0,
        stdout: answer.stdout ?? '',
        stderr: '',
        isStdoutTruncated: false,
        isStderrTruncated: false,
      },
    }
  })
  return runs
}

// Nothing runs beneath a test's hooks, so a tool call settles on the test's
// own bottom; this installs one, answering every tool with what `answer`
// holds when the call reaches it (the caller mutates it between calls).
function toolBottom(on: On, answer: { value: ToolCallResult } = { value: { result: 'ok' } }): void {
  on('tool.call', ($, e, next) => answer.value)
}

// A tool call as the engine raises one, in the main loop or a subagent's.
function toolCall(agentId?: string) {
  return { tool: 'Bash' as const, command: 'ls', ...(agentId !== undefined ? { agentId } : {}) }
}

// A main-scope agent-spawn call, the one call the headroom nudge computes on.
function spawnCall(agentId?: string) {
  return {
    tool: 'Agent' as const,
    description: 'look',
    prompt: 'look around',
    ...(agentId !== undefined ? { agentId } : {}),
  }
}

// The end of a subagent's run, as `turn.complete` carries it.
function agentEnd(agentId: string) {
  return {
    reason: 'answer' as const,
    answer: '',
    durationMs: 1,
    isAborted: false,
    turnId: `t-${agentId}`,
    agentId,
  }
}

// A subagent's stop, as the engine raises `classic.SubagentStop`.
function subagentStop(agentId: string) {
  return {
    agent_id: agentId,
    agent_transcript_path: `/home/u/.claude/projects/p/x/subagents/agent-${agentId}.jsonl`,
    agent_type: 'general-purpose',
    stop_hook_active: false,
  }
}

// The classic events a session raises after a tool call and at a subagent's
// stop. The mod hooks neither; a test that plays a session through raises
// them, and settles them on these bottoms.
function classicBottoms(on: On): void {
  on('classic.PostToolUse', ($, e, next) => ({}))
  on('classic.SubagentStop', ($, e, next) => ({}))
}

// The organization's policy plugin as Claude Code seats it for Team and
// Enterprise and on a machine with managed settings (`cc-plugin-sec-default`,
// outermost): every classic event, and the attachment, hook-context and
// compaction rows, a Skill's tool messages and a managed plugin's notes, go on
// past the person's plugins, as that plugin sends them. Each hook logs the
// links it passed over, which a session reports to the person as skipped hooks.
const POLICY: Plugin = {
  name: 'policy',
  tier: 'prepend',
  register(on) {
    on('classic.*', async ($, e, next) => {
      const settled = await next.to(e, 'append')
      for (const link of next.trace) {
        if (link.outcome === 'skipped') await $.ui.log(`${link.plugin}: ${next.event} skipped`)
      }
      return settled
    })
    on('session.append', { door: ['attachment', 'hook-context', 'compaction', 'tool-message', 'note'] }, async ($, e, next) => {
      const settled = await next.to(e, 'append')
      for (const link of next.trace) {
        if (link.outcome === 'skipped') await $.ui.log(`${link.plugin}: ${next.event} skipped`)
      }
      return settled
    })
  },
}

// The lines the policy plugin logged for the links it passed over.
function installSkipLog(on: On): string[] {
  const lines: string[] = []
  on('ui.log', ($, e, next) => {
    lines.push(e.text)
    return { value: undefined }
  })
  return lines
}

// A row at a door the policy plugin keeps from the person's plugins.
function reservedRow(door: 'attachment' | 'hook-context' | 'compaction' | 'tool-message' | 'note', uuid: string) {
  return {
    message: {
      type: 'attachment' as const,
      name: 'date_change',
      content: [{ type: 'text' as const, text: 'x' }],
    },
    door,
    origin: { kind: 'engine' as const },
    uuid,
  }
}

const ASK = { decision: 'ask' as const }
const QUESTION = [
  {
    question: 'Which lib?',
    header: 'Lib',
    options: [
      { label: 'date-fns', description: '' },
      { label: 'dayjs', description: '' },
    ],
    multiSelect: false,
  },
]

// The permission row's whole tree, as the terminal's table draws it; the
// engine's press handle varies per drawing, so it is pinned by type alone.
const permissionRow = (line1: string) => ({
  type: 'Box',
  props: { flexDirection: 'column' },
  children: [
    { type: 'Text', props: { wrap: 'truncate-end' }, children: [line1] },
    {
      type: 'Box',
      children: [
        { type: 'Text', children: ['  Allow this? Answer here or on your phone.   '] },
        { type: 'Button', props: { key: 'allow', label: 'Allow', hotkey: '1', plain: true }, press: { plugin: 'clauth', handle: expect.any(Number) } },
        { type: 'Text', children: ['   '] },
        { type: 'Button', props: { key: 'deny', label: 'Deny', hotkey: '2', plain: true }, press: { plugin: 'clauth', handle: expect.any(Number) } },
      ],
    },
  ],
  part: { plugin: 'clauth' },
})

const questionRow = (line1: string) => ({
  type: 'Box',
  props: { flexDirection: 'column' },
  children: [
    { type: 'Text', props: { wrap: 'truncate-end' }, children: [line1] },
    {
      type: 'Box',
      children: [
        { type: 'Text', children: ['  Answer on your phone, or here.   '] },
        { type: 'Button', props: { key: 'answer-here', label: 'Answer here', hotkey: '1', plain: true }, press: { plugin: 'clauth', handle: expect.any(Number) } },
        { type: 'Text', children: ['   Esc cancels'] },
      ],
    },
  ],
  part: { plugin: 'clauth' },
})

// --- decision table (group A) ---

test('A1 allow: a check the engine allows returns its verdict verbatim, no ask_opened', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ({ decision: 'allow', reason: 'allowed by rule', rule: 'Bash' }))
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(v.decision).toBe('allow')
  expect(v.reason).toBe('allowed by rule')
  expect(v.rule).toBe('Bash')
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A1 deny: a check the engine denies returns its verdict verbatim, no ask_opened', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ({ decision: 'deny', reason: 'blocked', rule: 'Bash(git push:*)' }))
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'git push' } })
  expect(v.decision).toBe('deny')
  expect(v.reason).toBe('blocked')
  expect(v.rule).toBe('Bash(git push:*)')
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A2: a held check resolves allow or deny from a phone answer', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  const p1 = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p1).toEqual({ decision: 'allow' })

  const p2 = $.tool.check({ tool: 'Bash', input: { command: 'rm -rf /' } })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { decision: 'deny' })
  await clock.advance(PARK_MS)
  expect(await p2).toEqual({ decision: 'deny' })
})

test('A3: hold:false returns the ask verdict, no hold, and closes the unheld ask by desk', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  expect(bridge.events.filter(e => e.kind === 'ask_opened')).toHaveLength(1)
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.find({ key: 'allow' })).toBeUndefined()
  await ui.unmount()
  expect(await p).toEqual({ decision: 'ask' })
  await clock.settle()
  // No hold to settle: the query's unheld ask is closed so the daemon stops
  // listing it.
  const closes = bridge.events.filter(e => e.kind === 'ask_closed')
  expect(closes.map(e => e.by)).toEqual(['desk'])
})

test('A4: a non-interactive session never holds and posts no ask_opened', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: null, isInteractive: false })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(v.decision).toBe('ask')
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A4b: a null surface never holds even when isInteractive is true', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: null, isInteractive: true })
  // The check must resolve now; if the surface guard is dropped the check is
  // held, and the race reports that instead of hanging the run.
  const v = await Promise.race([
    $.tool.check({ tool: 'Bash', input: { command: 'ls' } }),
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(v).toEqual({ decision: 'ask' })
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A5: an unreachable socket at ask_opened returns the ask verdict', async ($, on) => {
  installBoilerplate(on, { HOME: '/home/u' })
  const events: EventBody[] = []
  on('http.fetch', ($, e, next) => {
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined) events.push(body)
    if (e.url.includes('/poll') || e.url.includes('/wait')) return res(200, JSON.stringify({ remote: true, commands: [] }))
    if (body !== undefined && body.kind === 'ask_opened') throw new Error('no daemon')
    return res(200, JSON.stringify({ ok: true }))
  })
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(v).toEqual({ decision: 'ask' })
  expect(events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
})

test('A6: a mid-hold wait rejection hands the check to the engine dialog', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.wait.fail = 'reject'
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  // The check must resolve now; if the rejection was swallowed it stays held,
  // and the race reports that instead of hanging the run.
  const verdict = await Promise.race([
    p,
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(verdict).toEqual({ decision: 'ask' })
  const closes = bridge.events.filter(b => b.kind === 'ask_closed')
  expect(closes).toHaveLength(1)
  expect(closes[0]!.by).toBe('desk')
})

test('A6b: a mid-hold wait reporting remote:false hands the check to the engine dialog', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.wait.remote = false
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ decision: 'ask' })
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

test('A7: an input past the 1 MiB bound is never held', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const huge = 'x'.repeat(1024 * 1024)
  const v = await $.tool.check({ tool: 'Bash', input: { command: huge } })
  expect(v.decision).toBe('ask')
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A7b: a multibyte input over 1 MiB of bytes is never held', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const huge = 'é'.repeat(700_000) // 700k characters, 1.4M UTF-8 bytes
  const v = await $.tool.check({ tool: 'Bash', input: { command: huge } })
  expect(v.decision).toBe('ask')
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
})

test('A8: the first answer wins, desk or phone', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  // Desk first: press Allow, then a phone deny for the same ask is ignored.
  const p1 = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask1 = bridge.lastAsk()!
  const ui1 = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui1.press({ key: 'allow' })
  expect(await p1).toEqual({ decision: 'allow' })
  await ui1.unmount()
  bridge.answer(ask1, { decision: 'deny' })
  await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)

  // Phone first: mount the row, let the phone answer land, then press the
  // stale Allow; it must not resolve again or post a second close.
  const p2 = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await clock.settle()
  const ask2 = bridge.lastAsk()!
  const ui2 = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui2.find({ key: 'allow' })).toBeDefined()
  bridge.answer(ask2, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p2).toEqual({ decision: 'allow' })
  // The phone answer cleared the row, so the stale Allow press finds no Button
  // and the ask is not closed a second time.
  await expect(ui2.press({ key: 'allow' })).rejects.toThrow()
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
  await ui2.unmount()
})

test('A9: an answer naming a different ask never releases the held check', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask = bridge.lastAsk()!
  let settled = false
  void p.then(() => {
    settled = true
  })
  bridge.answer('some-other-ask', { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(settled).toBe(false)
  bridge.answer(ask, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ decision: 'allow' })
  expect(settled).toBe(true)
})

test('A10: an answers answer for a permission ask resolves it as a loss, not an allow', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: { q: 'y' } })
  await clock.advance(PARK_MS)
  // The wrong-form answer is a loss: the engine's own dialog draws, never the
  // check silently re-waiting on the closed ask.
  expect(await p).toEqual({ decision: 'ask' })
  const closes = bridge.events.filter(b => b.kind === 'ask_closed')
  expect(closes).toHaveLength(1)
  expect(closes[0]!.by).toBe('desk')
})

test('A12: two concurrent holds each resolve by their own ask id', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p1 = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  const p2 = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await clock.settle()
  const asks = bridge.events.filter(e => e.kind === 'ask_opened').map(e => e.ask as string)
  expect(asks).toHaveLength(2)
  bridge.answer(asks[1]!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p2).toEqual({ decision: 'allow' })
  bridge.answer(asks[0]!, { decision: 'deny' })
  await clock.advance(PARK_MS)
  const v1 = await Promise.race([
    p1,
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(v1).toEqual({ decision: 'deny' })
})

// --- trust boundary (group B) ---

test('B1: every fetch goes to the bridge socket at localhost', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })
  await $.session.append(appendRow('u1'))
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui.press({ key: 'deny' })
  await p
  await ui.unmount()
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await $.session.end({ reason: 'other', sessionId: 'sid-1', resume: { id: 'sid-1' } })

  expect(bridge.fetches.length).toBeGreaterThan(0)
  expect(bridge.fetches.some(f => f.url.startsWith('http://localhost/poll?session='))).toBe(true)
  expect(bridge.fetches.some(f => f.url.startsWith('http://localhost/wait?session='))).toBe(true)
  for (const f of bridge.fetches) {
    expect(f.init?.socketPath).toBe('/home/u/.clauth/bridge.sock')
    expect(f.url.startsWith('http://localhost/')).toBe(true)
  }
  const kinds = bridge.events.map(b => b.kind).sort()
  expect(kinds).toEqual(
    ['append', 'ask_closed', 'ask_opened', 'end', 'start', 'turn_end', 'turn_start'].sort(),
  )
})

test('B2: append wakes carry exactly session, seq, kind, uuid and agent_id', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.session.append(appendRow('u1'))
  await $.session.append(appendRow('u2', 'agent-1'))
  await clock.settle() // the wakes are fire-and-forget; let the chain drain
  const appends = bridge.events.filter(b => b.kind === 'append')
  expect(appends).toHaveLength(2)
  expect(Object.keys(appends[0]!).sort()).toEqual(['kind', 'seq', 'session', 'uuid'])
  expect(appends[0]!.uuid).toBe('u1')
  expect(Object.keys(appends[1]!).sort()).toEqual(['agent_id', 'kind', 'seq', 'session', 'uuid'])
  expect(appends[1]!.agent_id).toBe('agent-1')
})

test('B3: on Windows the mod is inert — no fetch at all', async ($, on) => {
  const { bridge } = setup(on, { HOME: '/home/u', OS: 'Windows_NT' })
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(v.decision).toBe('ask')
  await $.session.append(appendRow('u1'))
  expect(bridge.fetches).toHaveLength(0)
})

test('B4: with no HOME the mod is inert — no fetch at all', async ($, on) => {
  const { bridge } = setup(on, {})
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  expect(v.decision).toBe('ask')
  await $.session.append(appendRow('u1'))
  expect(bridge.fetches).toHaveLength(0)
})

// --- behaviour (group C) ---

test('C1: the desk row shows the tool and what it acts on, and nothing when not held', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  // Bash: the whole row, pinned by equality.
  const p1 = $.tool.check({ tool: 'Bash', input: { command: 'ls -la' } })
  await clock.settle()
  let ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.drawn()).toEqual(permissionRow('  Bash  ls -la'))
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  await p1

  // Write: its file_path.
  const p2 = $.tool.check({ tool: 'Write', input: { file_path: '/tmp/out.txt' } })
  await clock.settle()
  ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect((await ui.find({ type: 'Text', text: 'Write' }))?.text).toBe('  Write  /tmp/out.txt')
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  await p2

  // Any other tool: one-line JSON, cut at render with wrap truncate-end.
  const p3 = $.tool.check({ tool: 'Glob', input: { pattern: '**/*.ts', path: 'src' } })
  await clock.settle()
  ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(20) })
  const globText = await ui.find({ type: 'Text', text: 'Glob' })
  expect(globText?.text).toBe('  Glob  {"pattern":"**/*.ts","path":"src"}')
  expect(globText?.props?.wrap).toBe('truncate-end')
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  await p3

  // A multi-line Bash command collapses to its first line with an ellipsis.
  const p4 = $.tool.check({ tool: 'Bash', input: { command: "cat <<'EOF'\nls -la\nEOF" } })
  await clock.settle()
  ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect((await ui.find({ type: 'Text', text: 'Bash' }))?.text).toBe("  Bash  cat <<'EOF'…")
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  await p4

  // Nothing drawn when no check is held.
  ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.find({ type: 'Text', text: 'Allow this?' })).toBeUndefined()
  await ui.unmount()
})

test('C1q: the AskUserQuestion row shows the first question and its answer-here button', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.drawn()).toEqual(questionRow('  AskUserQuestion  Which lib?'))
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 'date-fns' } })
  await clock.advance(PARK_MS)
  await p
})

test('C1q-nl: a multi-line question collapses to its first line with an ellipsis', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({
    tool: 'AskUserQuestion',
    questions: [
      {
        question: 'First line\nsecond line',
        header: 'H',
        options: [
          { label: 'x', description: '' },
          { label: 'y', description: '' },
        ],
        multiSelect: false,
      },
    ],
  })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.drawn()).toEqual(questionRow('  AskUserQuestion  First line…'))
  await ui.unmount()
  bridge.answer(bridge.lastAsk()!, { answers: { 'First line\nsecond line': 'x' } })
  await clock.advance(PARK_MS)
  await p
})

test('C2: AskUserQuestion held answers from the phone without next', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 'date-fns' } })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ result: { questions: QUESTION, answers: { 'Which lib?': 'date-fns' } } })
  expect(nextRan).toBe(false)
})

test('C2n: a phone note reaches the tool result as its annotation', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 'date-fns' }, notes: { 'Which lib?': 'the small one' } })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({
    result: {
      questions: QUESTION,
      answers: { 'Which lib?': 'date-fns' },
      annotations: { 'Which lib?': { notes: 'the small one' } },
    },
  })
})

test('C2o: an answer with no notes carries no annotations', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 'date-fns' }, notes: {} })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ result: { questions: QUESTION, answers: { 'Which lib?': 'date-fns' } } })
})

test('C2p: a __proto__ question keeps its answer and its note', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const questions = [
    {
      question: '__proto__',
      header: 'Proto',
      options: [{ label: 'Red', description: '' }],
      multiSelect: false,
    },
  ]
  const p = $.tool.call({ tool: 'AskUserQuestion', questions })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, {
    answers: JSON.parse('{"__proto__":"Red"}') as Record<string, unknown>,
    notes: JSON.parse('{"__proto__":"the darker one"}') as Record<string, unknown>,
  })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({
    result: {
      questions,
      answers: JSON.parse('{"__proto__":"Red"}'),
      annotations: JSON.parse('{"__proto__":{"notes":"the darker one"}}'),
    },
  })
})

test('C2q: a note on an unanswered question reaches the tool result as its annotation', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const questions = [
    {
      question: 'Which lib?',
      header: 'Lib',
      options: [{ label: 'date-fns', description: '' }],
      multiSelect: false,
    },
    {
      question: 'Which format?',
      header: 'Format',
      options: [{ label: 'ISO', description: '' }],
      multiSelect: false,
    },
  ]
  const p = $.tool.call({ tool: 'AskUserQuestion', questions })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, {
    answers: { 'Which lib?': 'date-fns' },
    notes: { 'Which format?': 'pick for me' },
  })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({
    result: {
      questions,
      answers: { 'Which lib?': 'date-fns' },
      annotations: { 'Which format?': { notes: 'pick for me' } },
    },
  })
})

test('C2b: AskUserQuestion hold:false runs next so the engine draws its dialog', async ($, on) => {
  const { bridge } = setup(on, { HOME: '/home/u' }, false)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const r = await $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  expect(nextRan).toBe(true)
  expect(r).toEqual({ result: { answers: { q: 'engine' } } })
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(1)
})

test('C2c: pressing d releases the AskUserQuestion hold to the engine dialog', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui.press({ key: 'answer-here' })
  expect(await p).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  const closes = bridge.events.filter(b => b.kind === 'ask_closed')
  expect(closes).toHaveLength(1)
  expect(closes[0]!.by).toBe('desk')
  await ui.unmount()
})

test('C2d: a phone answer before d wins; the later d press does nothing', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 'date-fns' } })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ result: { questions: QUESTION, answers: { 'Which lib?': 'date-fns' } } })
  expect(nextRan).toBe(false)
  await expect(ui.press({ key: 'answer-here' })).rejects.toThrow()
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
  await ui.unmount()
})

test('C2e: pressing d first wins; a later phone answer is ignored', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  const ask = bridge.lastAsk()!
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui.press({ key: 'answer-here' })
  expect(await p).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  bridge.answer(ask, { answers: { 'Which lib?': 'date-fns' } })
  await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
  await ui.unmount()
})

test('C2f: an AskUserQuestion mid-hold wait rejection runs next', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.wait.fail = 'reject'
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  // The call must resolve now; if the rejection is swallowed it stays held, and
  // the race reports that instead of hanging the run.
  const v = await Promise.race([
    p,
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(v).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

test('C2g: a decision answer for a held AskUserQuestion resolves it as a loss', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  // The wrong-form answer is a loss: next runs so the engine draws its dialog.
  expect(await p).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

test('C2h: an answers value that is not a string resolves the question as a loss', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: { 'Which lib?': 42 } })
  await clock.advance(PARK_MS)
  // The non-string answer is a loss: next runs so the engine draws its dialog.
  expect(await p).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

for (const agentId of [undefined, 'sub-1']) {
  const scope = agentId === undefined ? 'the main loop' : 'a subagent'
  test(`C2i: an AskUserQuestion in ${scope} taken back at the desk draws the engine dialog with no second hold`, async ($, on) => {
    const { clock, bridge } = setup(on)
    // Claude Code answers its own permission check for AskUserQuestion `ask`
    // (the dialog is the question itself) and runs that check inside the tool
    // call's `next` (measured 2026-11-11 at the desk); the test kit turns an
    // unanswered `ask` into a denied call, which stands here for Claude Code
    // drawing its dialog.
    const checks: string[] = []
    on('tool.check', ($, e, next) => {
      checks.push(e.tool)
      return ASK
    })
    await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
    const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION, ...(agentId === undefined ? {} : { agentId }) })
    await clock.settle()
    const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
    await ui.press({ key: 'answer-here' })
    await clock.settle()
    expect(await ui.find({ key: 'allow' })).toBeUndefined()
    const r = await p
    expect(r.isError).toBe(true)
    expect(checks).toEqual(['AskUserQuestion'])
    const opened = bridge.events.filter(b => b.kind === 'ask_opened')
    expect(opened).toHaveLength(1)
    expect(opened[0]!.agent_id).toBe(agentId)
    await ui.unmount()
  })
}

test('C2j: a direct permission check for AskUserQuestion posts no ask_opened and returns the verdict as is', async ($, on) => {
  const { clock, bridge } = setup(on)
  const verdict = { decision: 'ask' as const, reason: 'the question itself' }
  on('tool.check', ($, e, next) => verdict)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'AskUserQuestion', input: { questions: QUESTION } })
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(0)
  expect(await p).toEqual(verdict)
})

test('C3: a prompt command submits once, queued when it arrives mid-turn', async ($, on) => {
  const { clock, bridge } = setup(on)
  const submissions: string[] = []
  on('prompt.submit', ($, e, next) => {
    submissions.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  bridge.poll.commands = [{ kind: 'prompt', text: 'hello' }]
  await clock.advance(PARK_MS)
  expect(submissions).toEqual(['hello'])

  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'mid' }]
  await clock.advance(PARK_MS)
  expect(submissions).toEqual(['hello'])
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  expect(submissions).toEqual(['hello', 'mid'])
})

test('C3b: a refused prompt holds the one behind it until its third refused try drops it', async ($, on) => {
  const { clock, bridge } = setup(on)
  const submissions: string[] = []
  on('prompt.submit', ($, e, next) => {
    if (e.text === 'boom') throw new Error('submit refused')
    submissions.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'boom', prompt: 'p1' },
    { kind: 'prompt', text: 'ok', prompt: 'p2' },
  ]
  // Its own arrival is boom's first try, ok's idle arrival its second.
  await clock.advance(PARK_MS)
  expect(submissions).toEqual([])

  await turn($, 't1')
  expect(submissions).toEqual(['ok'])
  await clock.settle()
  expect(promptReports(bridge)).toEqual([
    { prompt: 'p1', state: 'dropped', reason: 'refused' },
    { prompt: 'p2', state: 'delivered' },
  ])
})

test('C3d: a phone prompt reaches the model as the user, idle or queued', async ($, on) => {
  const { clock, bridge } = setup(on)
  const sent: Array<{ text: string; origin: unknown }> = []
  on('prompt.submit', ($, e, next) => {
    sent.push({ text: e.text, origin: e.origin })
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  bridge.poll.commands = [{ kind: 'prompt', text: 'idle' }]
  await clock.advance(PARK_MS)
  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'queued' }]
  await clock.advance(PARK_MS)
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })

  expect(sent.map(s => s.text)).toEqual(['idle', 'queued'])
  expect(sent.map(s => (s.origin as { asUser?: unknown } | undefined)?.asUser)).toEqual([true, true])
})

test('C3e: a Claude Code refusing asUser gets each phone prompt once without it, idle or queued', async ($, on) => {
  const { clock, bridge } = setup(on)
  const attempts: Array<{ text: string; asUser: unknown }> = []
  on('prompt.submit', ($, e, next) => {
    const asUser = (e.origin as { asUser?: unknown } | undefined)?.asUser
    attempts.push({ text: e.text, asUser })
    if (asUser !== undefined) throw new Error('unknown option asUser')
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  bridge.poll.commands = [{ kind: 'prompt', text: 'idle' }]
  await clock.advance(PARK_MS)
  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'queued' }]
  await clock.advance(PARK_MS)
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  await $.turn.start({ text: 'y', turnId: 't2' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't2' })

  expect(attempts).toEqual([
    { text: 'idle', asUser: true },
    { text: 'idle', asUser: undefined },
    { text: 'queued', asUser: true },
    { text: 'queued', asUser: undefined },
  ])
})

test('C3f: a Claude Code refusing both shapes gets two calls per delivery attempt, then the prompt once', async ($, on) => {
  const { clock, bridge } = setup(on)
  let refuse = true
  const attempts: Array<{ text: string; asUser: unknown }> = []
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    const asUser = (e.origin as { asUser?: unknown } | undefined)?.asUser
    attempts.push({ text: e.text, asUser })
    if (refuse) throw new Error('submit refused')
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  // Idle: two calls refused, so the prompt waits for turn.complete.
  bridge.poll.commands = [{ kind: 'prompt', text: 'idle' }]
  await clock.advance(PARK_MS)
  expect(attempts).toEqual([
    { text: 'idle', asUser: true },
    { text: 'idle', asUser: undefined },
  ])
  expect(deliveries).toEqual([])

  // Queued: the retry is two calls again, and the prompt is not dropped.
  await $.turn.start({ text: 'x', turnId: 't1' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  expect(attempts).toEqual([
    { text: 'idle', asUser: true },
    { text: 'idle', asUser: undefined },
    { text: 'idle', asUser: true },
    { text: 'idle', asUser: undefined },
  ])
  expect(deliveries).toEqual([])

  // The refusals stop: the next turn.complete delivers it once.
  refuse = false
  await $.turn.start({ text: 'y', turnId: 't2' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't2' })
  expect(deliveries).toEqual(['idle'])
  expect(attempts).toHaveLength(5)
})

test('C3c: a prompt refused while idle is queued for the next turn.complete', async ($, on) => {
  const { clock, bridge } = setup(on)
  let refuse = true
  const submissions: string[] = []
  on('prompt.submit', ($, e, next) => {
    if (refuse) throw new Error('submit refused')
    submissions.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  // Refused while idle: the device already got 200, so the prompt must be kept
  // for the next turn.complete instead of dropped.
  bridge.poll.commands = [{ kind: 'prompt', text: 'later' }]
  await clock.advance(PARK_MS)
  expect(submissions).toEqual([])

  refuse = false
  await $.turn.start({ text: 'x', turnId: 't1' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  expect(submissions).toEqual(['later'])
})

test('PQ1: a prompt refused every try is dropped on its third and the ones behind it still arrive', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    if (e.text.startsWith('/')) throw new Error('a text beginning with / would run a command as the user')
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: '/compact', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'mid', prompt: 'p2' }]
  await clock.advance(PARK_MS)
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  expect(deliveries).toEqual([])
  await turn($, 't2')
  expect(deliveries).toEqual(['mid'])
  await turn($, 't3')
  expect(deliveries).toEqual(['mid'])
})

test('PQ2: a newer idle prompt does not overtake an older queued one', async ($, on) => {
  const { clock, bridge } = setup(on)
  let refuse = true
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    if (refuse) return { drop: HELD_DROPS[0] }
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: 'A', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  refuse = false
  // B's idle arrival drains the head: A goes first, B waits for A's turn.
  bridge.poll.commands = [{ kind: 'prompt', text: 'B', prompt: 'p2' }]
  await clock.advance(PARK_MS)
  expect(deliveries).toEqual(['A'])
  await turn($, 't1')
  expect(deliveries).toEqual(['A', 'B'])
})

test('PQ3: a turn.complete while the head submit waits behind a desk turn delivers each prompt once', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', async ($, e, next) => {
    if (e.text === 'A') await clock.sleep(2 * PARK_MS)
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'A', prompt: 'p1' },
    { kind: 'prompt', text: 'B', prompt: 'p2' },
  ]
  await clock.advance(PARK_MS)
  const first = $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
  await clock.settle()
  await $.turn.start({ text: 'desk', turnId: 't1' })
  const second = $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  await clock.settle()
  await clock.advance(3 * PARK_MS)
  await first
  await second
  await turn($, 't2')
  expect(deliveries).toEqual(['A', 'B'])
})

test('PQ4: overlapping main turn.completes deliver the waiting prompt once', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', async ($, e, next) => {
    await clock.sleep(2 * PARK_MS)
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'A', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  const first = $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
  await clock.settle()
  await $.turn.start({ text: 'desk', turnId: 't1' })
  const second = $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  await clock.settle()
  await clock.advance(3 * PARK_MS)
  await first
  await second
  expect(deliveries).toEqual(['A'])
})

for (const reason of HELD_DROPS) {
  test(`PQ5: a ${JSON.stringify(reason)} drop is a refused try, one call each, and the third drops the prompt`, async ($, on) => {
    const { clock, bridge } = setup(on)
    const calls: string[] = []
    on('prompt.submit', ($, e, next) => {
      calls.push(e.text)
      return { drop: reason }
    })
    await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
    bridge.poll.commands = [{ kind: 'prompt', text: 'x', prompt: 'p1' }]
    await clock.advance(PARK_MS)
    await turn($, 't1')
    await clock.settle()
    expect(promptReports(bridge)).toEqual([])
    await turn($, 't2')
    await clock.settle()
    expect(calls).toEqual(['x', 'x', 'x'])
    expect(promptReports(bridge)).toEqual([{ prompt: 'p1', state: 'dropped', reason: 'refused' }])
    await turn($, 't3')
    expect(calls).toHaveLength(3)
  })
}

// The kit refuses a hook returning a non-string drop, so the engine's `true`
// drop during teardown has no case here.
for (const drop of [REMOVED_DROP, 'an earlier prompt in the batch failed']) {
  test(`PQ6: a ${JSON.stringify(drop)} drop is final at once, never resent, and the next prompt goes in the same drain`, async ($, on) => {
    const { clock, bridge } = setup(on)
    const calls: string[] = []
    on('prompt.submit', ($, e, next) => {
      calls.push(e.text)
      if (e.text === 'gone') return { drop }
      return { text: e.text }
    })
    await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
    await $.turn.start({ text: 'x', turnId: 't0' })
    bridge.poll.commands = [
      { kind: 'prompt', text: 'gone', prompt: 'p1' },
      { kind: 'prompt', text: 'next', prompt: 'p2' },
    ]
    await clock.advance(PARK_MS)
    await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
    await turn($, 't1')
    await clock.settle()
    expect(calls).toEqual(['gone', 'next'])
    expect(promptReports(bridge)).toEqual([
      { prompt: 'p1', state: 'dropped', reason: 'removed' },
      { prompt: 'p2', state: 'delivered' },
    ])
  })
}

// A `/clear` ends the conversation and fires no `session.start`, while the
// process goes on under a new session id: the bridge registers that id and
// keeps polling under it, so the phone still reaches the process.
// A `/resume` inside a running process ends the conversation the same way: the
// process goes on under another id.
for (const reason of ['clear', 'resume'] as const) {
  test(`CL1: after a /${reason} the bridge registers the new session id and a phone prompt still arrives`, async ($, on) => {
    const session = { down: false, id: 'sid-1' }
    const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
    const calls: string[] = []
    on('prompt.submit', ($, e, next) => {
      calls.push(e.text)
      return { text: e.text }
    })
    await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
    await clock.advance(PARK_MS)
    await $.session.end({ sessionId: 'sid-1', reason })
    session.id = 'sid-2'
    const fetchesAtClear = bridge.fetches.length
    for (let i = 0; i < 5; i++) await clock.advance(PARK_MS)
    const starts = bridge.events.filter(b => b.kind === 'start')
    expect(starts.map(b => b.session)).toEqual(['sid-1', 'sid-2'])
    const pollsAfter = bridge.fetches.slice(fetchesAtClear).filter(f => f.url.includes('/poll?'))
    expect(pollsAfter.length).toBeGreaterThan(0)
    for (const f of pollsAfter) expect(f.url).toContain('/poll?session=sid-2')
    bridge.poll.commands = [{ kind: 'prompt', text: 'after clear', prompt: 'p1' }]
    for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
    await clock.settle()
    expect(calls).toEqual(['after clear'])
  })
}

test('CL3: the clear end goes out under the ended id, and the new id start carries the session.start fields', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'end').map(b => b.session)).toEqual(['sid-1'])
  const starts = bridge.events.filter(b => b.kind === 'start').map(({ seq, ...rest }) => rest)
  expect(starts).toEqual([
    { session: 'sid-1', kind: 'start', cwd: '/w', interactive: true, prompt_reports: true },
    { session: 'sid-2', kind: 'start', cwd: '/w', interactive: true, prompt_reports: true },
  ])
})

// Should `$.session.id()` still read the ended id for a while after a `/clear`,
// the mod waits for it to move.
test('CL4: while the id still reads the cleared one nothing polls or posts, and no second start goes out', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await clock.settle()
  const at = bridge.fetches.length
  await $.turn.start({ text: 'x', turnId: 't1' })
  for (let i = 0; i < 4; i++) await clock.advance(PARK_MS)
  expect(bridge.fetches.slice(at)).toEqual([])
  expect(bridge.events.filter(b => b.kind === 'start')).toHaveLength(1)
})

test('CL5: a refused start keeps the new id unpolled and retries after the backoff', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
  bridge.start.refuse = 'sid-2'
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  expect(bridge.fetches.filter(f => f.url.includes('/poll?session=sid-2'))).toHaveLength(0)
  expect(bridge.events.filter(b => b.kind === 'start' && b.session === 'sid-2')).toHaveLength(1)
  bridge.start.refuse = null
  for (let i = 0; i < 8; i++) await clock.advance(PARK_MS)
  expect(bridge.fetches.filter(f => f.url.includes('/poll?session=sid-2')).length).toBeGreaterThan(0)
})

test('CL6: an end with no session.start before it posts nothing', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await clock.settle()
  expect(bridge.fetches).toEqual([])
})

// A check held across a `/clear` waits under the session it was opened in (the
// real daemon has closed that session, so there the wait settles the hold as a
// loss; this fake keeps the ask open and the test reads only the URLs).
test('CL7: a held check waits under the session it opened in, across a /clear', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  void $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 4; i++) await clock.advance(PARK_MS)
  const waits = bridge.fetches.filter(f => f.url.includes('/wait?'))
  expect(waits.length).toBeGreaterThan(1)
  for (const f of waits) expect(f.url).toContain('/wait?session=sid-1&')
})

// Any other end is the process leaving: the poll loop stops for good.
for (const reason of ['prompt_input_exit', 'logout', 'other'] as const) {
  test(`CL2: a session end for ${reason} stops polling`, async ($, on) => {
    const session = { down: false, id: 'sid-1' }
    const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
    await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
    await clock.advance(PARK_MS)
    await $.session.end({ sessionId: 'sid-1', reason })
    // Even a moved id is no next conversation once the process is leaving.
    session.id = 'sid-2'
    const fetchesAtEnd = bridge.fetches.length
    for (let i = 0; i < 5; i++) await clock.advance(PARK_MS)
    expect(bridge.fetches.slice(fetchesAtEnd).filter(f => f.url.includes('/poll?'))).toHaveLength(0)
    expect(bridge.events.filter(b => b.kind === 'start').map(b => b.session)).toEqual(['sid-1'])
  })
}

// A read of the session id that fails is no conversation change: the poll goes
// on under the id it registered.
test('CL8: a session id read that throws keeps polling under the registered id', async ($, on) => {
  const session = { down: false }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  session.down = true
  const at = bridge.fetches.length
  for (let i = 0; i < 2; i++) await clock.advance(PARK_MS)
  const polls = bridge.fetches.slice(at).filter(f => f.url.includes('/poll?'))
  expect(polls.length).toBeGreaterThan(0)
  for (const f of polls) expect(f.url).toContain('/poll?session=sid-1&')
})

// An ask held across a `/clear` belongs to the session it opened in, its close
// too, even once the poll re-registered the next one.
test('CL9: a held ask closed at the desk after the re-register closes under the session it opened in', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, true, session)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'start').map(b => b.session)).toEqual(['sid-1', 'sid-2'])
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui.press({ key: 'deny' })
  await p
  await ui.unmount()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => b.session)).toEqual(['sid-1'])
})

// A daemon fake keyed on the pid's session record, as the daemon resolves each
// request: an event, poll or wait naming another id answers 403
// `session_mismatch`. `gate` holds one session's `start` open until released.
function recordBridge(on: On, clock: { sleep: (ms: number) => Promise<void> }) {
  const st = {
    record: 'sid-1',
    fetches: [] as string[],
    events: [] as EventBody[],
    pending: {} as Record<string, unknown[]>,
    gate: null as null | { session: string; release: (() => void) | null },
  }
  const sessionOf = (url: string) => decodeURIComponent((url.split('session=')[1] ?? '').split('&')[0] ?? '')
  on('http.fetch', async ($, e, next) => {
    st.fetches.push(e.url)
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined) st.events.push(body)
    if (e.url.includes('/poll')) {
      const s = sessionOf(e.url)
      if (s !== st.record) return res(403, JSON.stringify({ error: 'session_mismatch' }))
      const take = () => {
        const commands = st.pending[s] ?? []
        st.pending[s] = []
        return res(200, JSON.stringify({ remote: true, commands }))
      }
      if ((st.pending[s] ?? []).length > 0) return take()
      await clock.sleep(PARK_MS)
      return take()
    }
    const gate = st.gate
    if (body !== undefined && body.kind === 'start' && gate !== null && body.session === gate.session) {
      await new Promise<void>(release => {
        gate.release = release
      })
    }
    if (e.url.includes('/wait')) {
      if (sessionOf(e.url) !== st.record) return res(403, JSON.stringify({ error: 'session_mismatch' }))
      await clock.sleep(PARK_MS)
      return res(200, JSON.stringify({ remote: true, answer: null, closed: false }))
    }
    if (body !== undefined && body.session !== st.record) {
      return res(403, JSON.stringify({ error: 'session_mismatch' }))
    }
    if (body !== undefined && body.kind === 'ask_opened') {
      return res(200, JSON.stringify({ ok: true, remote: true, hold: true }))
    }
    return res(200, JSON.stringify({ ok: true }))
  })
  return st
}

// Two `/clear`s inside one start round trip: the id the second one ended never
// becomes the id the poll speaks for, so its phone prompt reaches no later
// conversation.
test('CL10: an id that ended while its start was out is not adopted', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock } = installBoilerplate(on, { HOME: '/home/u' }, session)
  const st = recordBridge(on, clock)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  st.record = 'sid-2'
  const gate = { session: 'sid-2', release: null as (() => void) | null }
  st.gate = gate
  await clock.advance(PARK_MS)
  await clock.advance(PARK_MS)
  expect(gate.release).not.toBeNull()
  // The second /clear lands while start(sid-2) is still out; the record lags.
  await $.session.end({ sessionId: 'sid-2', reason: 'clear' })
  session.id = 'sid-3'
  st.pending['sid-2'] = [{ kind: 'prompt', text: 'meant for sid-2', prompt: 'p2' }]
  gate.release!()
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  await clock.settle()
  expect(calls).toEqual([])
  const polled = st.fetches.filter(f => f.includes('/poll?')).map(f => f.split('session=')[1]?.split('&')[0])
  expect(polled).not.toContain('sid-2')
})

// Should the id move before `session.end` fires, the poll registers the next
// id first and its start round trip spans the end: that poll's commands belong
// to the next conversation and still run.
test('CL11: commands for the next id survive an end landing while its start is out', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock } = installBoilerplate(on, { HOME: '/home/u' }, session)
  const st = recordBridge(on, clock)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  session.id = 'sid-2'
  st.record = 'sid-2'
  const gate = { session: 'sid-2', release: null as (() => void) | null }
  st.gate = gate
  await clock.advance(PARK_MS)
  await clock.advance(PARK_MS)
  expect(gate.release).not.toBeNull()
  const ending = $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await clock.settle()
  st.pending['sid-2'] = [{ kind: 'prompt', text: 'for sid-2', prompt: 'p2' }]
  gate.release!()
  await ending
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  await clock.settle()
  expect(calls).toEqual(['for sid-2'])
})

// The daemon refuses a held ask's `/wait` once the record moved: the hold
// settles as a loss and the check falls back to Claude Code's own ask.
test('CL12: a check held across a /clear settles as a loss once the record moves', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock } = installBoilerplate(on, { HOME: '/home/u' }, session)
  const st = recordBridge(on, clock)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  let verdict: unknown = 'pending'
  void $.tool.check({ tool: 'Bash', input: { command: 'ls' } }).then(v => {
    verdict = v
  })
  await clock.settle()
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  st.record = 'sid-2'
  for (let i = 0; i < 4; i++) await clock.advance(PARK_MS)
  await clock.settle()
  expect(st.events.filter(b => b.kind === 'ask_opened').map(b => b.session)).toEqual(['sid-1'])
  expect(verdict).toEqual(ASK)
  expect(st.events.filter(b => b.kind === 'ask_closed').map(b => b.session)).toEqual(['sid-1'])
})

// An ask the daemon did not hold goes to Claude Code's own dialog: it closes
// once the desk settled it, so the daemon's open asks (and the once-per-wait
// `blocked` push they gate) do not keep it forever.
for (const agentId of [undefined, 'agent-1']) {
  const scope = agentId === undefined ? 'the main loop' : 'a subagent'
  test(`U1: an unheld permission ask in ${scope} closes by desk once its tool call settles`, async ($, on) => {
    const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
    on('tool.check', ($, e, next) => ASK)
    await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
    await $.tool.call({ ...toolCall(agentId), tool_use_id: 'tu-1' })
    await clock.settle()
    const opened = bridge.events.filter(b => b.kind === 'ask_opened')
    expect(opened).toHaveLength(1)
    const closed = bridge.events.filter(b => b.kind === 'ask_closed')
    expect(closed.map(b => ({ ask: b.ask, by: b.by, session: b.session }))).toEqual([
      { ask: opened[0]!.ask, by: 'desk', session: 'sid-1' },
    ])
  })
}

test('U2: an unheld AskUserQuestion closes by desk once the desk answered it', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => ({ result: { answers: { q: 'engine' } } }))
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const r = await $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  expect(r).toEqual({ result: { answers: { q: 'engine' } } })
  const opened = bridge.events.filter(b => b.kind === 'ask_opened')
  expect(opened).toHaveLength(1)
  const closed = bridge.events.filter(b => b.kind === 'ask_closed')
  expect(closed.map(b => ({ ask: b.ask, by: b.by }))).toEqual([{ ask: opened[0]!.ask, by: 'desk' }])
})

// A permission query (no tool call behind it) shows no dialog to wait on.
test('U3: an unheld check with no tool call behind it closes at once', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  expect(v).toEqual(ASK)
  const opened = bridge.events.filter(b => b.kind === 'ask_opened')
  expect(opened).toHaveLength(1)
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => b.ask)).toEqual([opened[0]!.ask])
})

// A held ask the phone answered was closed by the daemon on that answer: the
// mod posts no close for it, from its hold or from the unheld path.
test('U4: a held ask that the phone answered posts no close', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ ...toolCall(), tool_use_id: 'tu-2' })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { decision: 'deny' })
  await clock.advance(PARK_MS)
  await p
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
})

// The unheld question stays open while Claude Code's own dialog is up, and its
// close goes under the session it opened in even once a `/clear` moved the
// module on to the next one.
test('U5: an unheld question answered after a /clear closes then, under the session it opened in', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false, session)
  let answerDesk: (() => void) | null = null
  on('tool.call', { tool: 'AskUserQuestion' }, async ($, e, next) => {
    await new Promise<void>(release => {
      answerDesk = release
    })
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  expect(answerDesk).not.toBeNull()
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'start').map(b => b.session)).toEqual(['sid-1', 'sid-2'])
  answerDesk!()
  await p
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => [b.by, b.session])).toEqual([['desk', 'sid-1']])
})

// An `ask_opened` the daemon refused recorded nothing: there is no ask to close.
test('U6: a refused ask_opened posts no close', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  bridge.opened.refuse = true
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const v = await $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await $.tool.call({ ...toolCall(), tool_use_id: 'tu-3' })
  await clock.settle()
  expect(v).toEqual(ASK)
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(2)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
})

// Claude Code's own permission dialog, as the test kit lets a test answer an
// ask: a classic PermissionRequest hook, held open until the test answers it.
function gatedDialog(on: On) {
  const answers: Array<(behavior: 'allow' | 'deny') => void> = []
  on('classic.PermissionRequest', async ($, e, next) => {
    const behavior = await new Promise<'allow' | 'deny'>(answer => answers.push(answer))
    return { decision: behavior === 'allow' ? { behavior } : { behavior, message: 'no' } }
  })
  return answers
}

test('U7: an unheld permission ask stays open while the desk dialog is up, closes once answered', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  on('tool.check', ($, e, next) => ASK)
  const dialog = gatedDialog(on)
  toolBottom(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ ...toolCall(), tool_use_id: 'tu-1' })
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_opened')).toHaveLength(1)
  expect(dialog).toHaveLength(1)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(0)
  dialog[0]!('allow')
  expect(await p).toEqual({ result: 'ok' })
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => b.by)).toEqual(['desk'])
})

// Each unheld ask closes with its own call: a subagent's dialog answered
// first closes only its own ask.
test('U8: concurrent unheld asks close per call, the second answered first', async ($, on) => {
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false)
  on('tool.check', ($, e, next) => ASK)
  const dialog = gatedDialog(on)
  toolBottom(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const pa = $.tool.call({ ...toolCall(), tool_use_id: 'tu-a' })
  await clock.settle()
  const pb = $.tool.call({ ...toolCall('agent-1'), tool_use_id: 'tu-b' })
  await clock.settle()
  const opened = bridge.events.filter(b => b.kind === 'ask_opened')
  expect(opened.map(o => o.agent_id ?? null)).toEqual([null, 'agent-1'])
  expect(dialog).toHaveLength(2)
  dialog[1]!('deny')
  await pb
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => b.ask)).toEqual([opened[1]!.ask])
  dialog[0]!('allow')
  await pa
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => b.ask)).toEqual([opened[1]!.ask, opened[0]!.ask])
})

test('U9: an unheld permission ask answered after a /clear closes under the session it opened in', async ($, on) => {
  const session = { down: false, id: 'sid-1' }
  const { clock, bridge } = setup(on, { HOME: '/home/u' }, false, session)
  on('tool.check', ($, e, next) => ASK)
  const dialog = gatedDialog(on)
  toolBottom(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ ...toolCall(), tool_use_id: 'tu-1' })
  await clock.settle()
  expect(dialog).toHaveLength(1)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  session.id = 'sid-2'
  for (let i = 0; i < 3; i++) await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'start').map(b => b.session)).toEqual(['sid-1', 'sid-2'])
  dialog[0]!('allow')
  await p
  await clock.settle()
  expect(bridge.events.filter(b => b.kind === 'ask_closed').map(b => [b.by, b.session])).toEqual([['desk', 'sid-1']])
})

// A `/clear` ends the conversation and fires no `session.start`: the next
// conversation's turns reach the same module directly.
test('PQ7: a conversation end clears the queue, so nothing queued reaches the next conversation', async ($, on) => {
  const { clock, bridge } = setup(on)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'old-a', prompt: 'p1' },
    { kind: 'prompt', text: 'old-b', prompt: 'p2' },
  ]
  await clock.advance(PARK_MS)
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await turn($, 't1')
  await clock.settle()
  expect(calls).toEqual([])
  // The daemon reports the dropped prompts itself once it takes the end.
  expect(promptReports(bridge)).toEqual([])
})

test('PQ8: a delivered prompt is reported by its id, and one with no id reports nothing', async ($, on) => {
  const { clock, bridge } = setup(on)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: 'old-daemon' }]
  await clock.advance(PARK_MS)
  await turn($, 't1')
  bridge.poll.commands = [{ kind: 'prompt', text: 'idle', prompt: 'p7' }]
  await clock.advance(PARK_MS)
  await clock.settle()
  expect(calls).toEqual(['old-daemon', 'idle'])
  expect(promptReports(bridge)).toEqual([{ prompt: 'p7', state: 'delivered' }])
})

test('PQ9: the start event tells the daemon this mod reports prompt outcomes', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await clock.settle()
  const starts = bridge.events.filter(e => e.kind === 'start')
  expect(starts.map(({ seq, ...rest }) => rest)).toEqual([
    { session: 'sid-1', kind: 'start', cwd: '/home/u', interactive: true, prompt_reports: true },
  ])
})

test('PQ14: a drain stops at each delivery, so the next prompt waits for that prompt own turn', async ($, on) => {
  const { clock, bridge } = setup(on)
  const calls: string[] = []
  const aborts: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  on('turn.abort', ($, e, next) => {
    aborts.push(e.turnId)
    return { value: undefined }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'A', prompt: 'p1' },
    { kind: 'prompt', text: 'B', prompt: 'p2' },
  ]
  await clock.advance(PARK_MS)
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
  expect(calls).toEqual(['A'])
  // A's turn runs: the mod sees it, so a phone interrupt reaches it.
  await $.turn.start({ text: 'A', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  expect(aborts).toEqual(['t1'])
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: true, turnId: 't1' })
  expect(calls).toEqual(['A', 'B'])
})

test('PQ15: every poll tells the daemon this mod reports prompt outcomes', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await clock.advance(PARK_MS)
  await clock.advance(PARK_MS)
  const polls = bridge.fetches.filter(f => f.url.includes('/poll?'))
  expect(polls.length).toBeGreaterThan(1)
  for (const f of polls) {
    expect(f.url).toContain('prompt_reports=1')
  }
})

// A fresh load of the module (a hooks-worker respawn or a reload) starts with
// empty module state; the queue it resumes is the one in `$.state`.
const PROMPTS = { plugin: 'clauth', key: 'prompts' }

function seedPrompts(on: On, stored: unknown) {
  on('state.get', async ($, e, next) => {
    const r = (await next(e)) as { value?: { value?: unknown; version: number } }
    if (e.plugin === PROMPTS.plugin && e.key === PROMPTS.key && r?.value?.value === undefined) {
      return { value: { value: stored, version: r!.value!.version } }
    }
    return r as never
  })
}

test('PQ16: a fresh load resumes the stored queue in order at the next turn.complete', async ($, on) => {
  seedPrompts(on, [
    { text: 'kept-a', id: 'p1', tries: 0 },
    { text: 'kept-b', id: 'p2', tries: 1 },
  ])
  const { clock, bridge } = setup(on)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await clock.settle()
  expect(calls).toEqual([])
  await turn($, 't1')
  expect(calls).toEqual(['kept-a'])
  await turn($, 't2')
  await clock.settle()
  expect(calls).toEqual(['kept-a', 'kept-b'])
  expect(promptReports(bridge)).toEqual([
    { prompt: 'p1', state: 'delivered' },
    { prompt: 'p2', state: 'delivered' },
  ])
})

test('PQ17: a prompt that was mid-submit at the reload is reported unknown, never resent', async ($, on) => {
  seedPrompts(on, [
    { text: 'half', id: 'p1', tries: 0, submitting: true },
    { text: 'next', id: 'p2', tries: 0 },
  ])
  const { clock, bridge } = setup(on)
  const calls: string[] = []
  on('prompt.submit', ($, e, next) => {
    calls.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await turn($, 't1')
  await clock.settle()
  expect(calls).toEqual(['next'])
  expect(promptReports(bridge)).toEqual([
    { prompt: 'p1', state: 'dropped', reason: 'unknown' },
    { prompt: 'p2', state: 'delivered' },
  ])
})

test('PQ19: a poll reply landing while the end saves the cleared queue runs none of its commands', async ($, on) => {
  let slowClear = false
  let wait: (ms: number) => Promise<void> = async () => {}
  on('state.set', async ($, e, next) => {
    const cleared = Array.isArray(e.value) && e.value.length === 0
    if (slowClear && e.plugin === PROMPTS.plugin && e.key === PROMPTS.key && cleared) await wait(2 * PARK_MS)
    return next(e)
  })
  const { clock, bridge } = setup(on)
  wait = ms => clock.sleep(ms)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  // The first poll answers empty; the next one parks and answers mid-end.
  await clock.advance(PARK_MS)
  bridge.poll.commands = [{ kind: 'prompt', text: 'stale', prompt: 'p1' }]
  slowClear = true
  const ending = $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  for (let n = 0; n < 6; n++) {
    await clock.settle()
    await clock.advance(PARK_MS)
  }
  await clock.settle()
  expect(deliveries).toEqual([])
  expect(promptReports(bridge)).toEqual([])
  await ending
})

test('PQ20: a turn.complete landing while a drain saves after a delivery still drains the next prompt', async ($, on) => {
  let wait: (ms: number) => Promise<void> = async () => {}
  on('state.set', async ($, e, next) => {
    const v = e.value as { text?: string; submitting?: true }[] | undefined
    const onlyB = Array.isArray(v) && v.length === 1 && v[0].text === 'B' && v[0].submitting === undefined
    if (e.plugin === PROMPTS.plugin && e.key === PROMPTS.key && onlyB) await wait(2 * PARK_MS)
    return next(e)
  })
  const { clock, bridge } = setup(on)
  wait = ms => clock.sleep(ms)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'A', prompt: 'p1' },
    { kind: 'prompt', text: 'B', prompt: 'p2' },
  ]
  await clock.advance(PARK_MS)
  // A is delivered, and its whole turn runs while the drain still saves [B].
  const first = $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
  await clock.settle()
  await turn($, 'tA')
  await clock.advance(3 * PARK_MS)
  await first
  await clock.settle()
  expect(deliveries).toEqual(['A', 'B'])
})

test('PQ22: a prompt arriving while the head submit is out waits for the delivered prompt own turn', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', async ($, e, next) => {
    if (e.text === 'A') await clock.sleep(2 * PARK_MS)
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: 'A', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  bridge.poll.commands = [{ kind: 'prompt', text: 'B', prompt: 'p2' }]
  await clock.advance(PARK_MS)
  await clock.advance(2 * PARK_MS)
  await clock.settle()
  expect(deliveries).toEqual(['A'])
  await turn($, 'tA')
  await clock.settle()
  expect(deliveries).toEqual(['A', 'B'])
})

test('PQ21: after a fresh load an idle phone prompt starts the drain, oldest stored prompt first', async ($, on) => {
  seedPrompts(on, [{ text: 'kept', id: 'p1', tries: 0 }])
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: 'new', prompt: 'p2' }]
  await clock.advance(PARK_MS)
  await clock.settle()
  expect(deliveries).toEqual(['kept'])
  await turn($, 't1')
  expect(deliveries).toEqual(['kept', 'new'])
})

test('PQ18: the queue is written to $.state as it changes, so a fresh load finds it', async ($, on) => {
  const writes: unknown[] = []
  on('state.set', async ($, e, next) => {
    if (e.plugin === PROMPTS.plugin && e.key === PROMPTS.key) writes.push(e.value)
    return next(e)
  })
  const { clock, bridge } = setup(on)
  on('prompt.submit', ($, e, next) => ({ text: e.text }))
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't0' })
  bridge.poll.commands = [{ kind: 'prompt', text: 'w', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  await clock.settle()
  expect(writes.at(-1)).toEqual([{ text: 'w', id: 'p1', tries: 0 }])
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't0' })
  await clock.settle()
  expect(writes.at(-1)).toEqual([])
})

test('PQ11: a slow prompt submit never holds the commands of the polls after it', async ($, on) => {
  const { clock, bridge } = setup(on)
  const aborts: string[] = []
  on('prompt.submit', async ($, e, next) => {
    if (e.text === 'slow') await clock.sleep(3 * PARK_MS)
    return { text: e.text }
  })
  on('turn.abort', ($, e, next) => {
    aborts.push(e.turnId)
    return { value: undefined }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [{ kind: 'prompt', text: 'slow', prompt: 'p1' }]
  await clock.advance(PARK_MS)
  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  expect(aborts).toEqual(['t1'])
  await clock.advance(3 * PARK_MS)
})

test('PQ12: a poll reply that lands after the conversation ended runs none of its commands', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    deliveries.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  // The first poll parks; its reply is still on the way when the conversation ends.
  await clock.advance(PARK_MS)
  bridge.poll.commands = [{ kind: 'prompt', text: 'stale', prompt: 'p1' }]
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await clock.advance(PARK_MS)
  await clock.settle()
  expect(deliveries).toEqual([])
  expect(promptReports(bridge)).toEqual([])
})

test('PQ13: a conversation ending while a reply command waits runs none of the commands after it', async ($, on) => {
  const { clock, bridge } = setup(on)
  const deliveries: string[] = []
  on('prompt.submit', ($, e, next) => {
    deliveries.push(e.text)
    return { text: e.text }
  })
  on('turn.abort', async ($, e, next) => {
    await clock.sleep(3 * PARK_MS)
    return { value: undefined }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'interrupt' }, { kind: 'prompt', text: 'late', prompt: 'p9' }]
  await clock.advance(PARK_MS)
  // The interrupted turn ends while its abort call is still out, then the
  // person clears the idle session.
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: true, turnId: 't1' })
  await $.session.end({ sessionId: 'sid-1', reason: 'clear' })
  await clock.advance(3 * PARK_MS)
  await clock.settle()
  expect(deliveries).toEqual([])
  expect(promptReports(bridge)).toEqual([])
})

test('C4: an interrupt aborts the running turn, and does nothing with none running', async ($, on) => {
  const { clock, bridge } = setup(on)
  const aborts: string[] = []
  on('turn.abort', ($, e, next) => {
    aborts.push(e.turnId)
    return { value: undefined }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  expect(aborts).toEqual([])

  await $.turn.start({ text: 'x', turnId: 't1' })
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  expect(aborts).toEqual(['t1'])
})

test('C5: event sequence numbers come from $.state, so a module-variable seq is caught', async ($, on) => {
  // Seed the plugin's seq through a state.get hook: the store holds nothing, so
  // the first read returns 41. A seq read out of $.state continues from it; one
  // kept in a module variable ignores it and starts over at 1.
  on('state.get', async ($, e, next) => {
    const r = (await next(e)) as { value?: { value?: unknown; version: number } }
    if (e.plugin === 'clauth' && e.key === 'seq' && r?.value?.value === undefined) {
      return { value: { value: 41, version: r!.value!.version } }
    }
    return r as never
  })
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.session.append(appendRow('u1'))
  await clock.settle() // the wakes are fire-and-forget; let the chain drain
  const seqs = bridge.events.map(b => b.seq as number)
  expect(seqs).toEqual([42, 43])
})

test('C6: an errored poll waits a bounded backoff before the next poll', async ($, on) => {
  const { clock, bridge } = setup(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const polls = () => bridge.fetches.filter(f => f.url.includes('/poll')).length

  bridge.poll.fail = 'error'
  await clock.advance(1000)
  expect(polls()).toBe(1)

  await clock.advance(4000)
  expect(polls()).toBe(1)

  bridge.poll.fail = null
  await clock.advance(1000)
  expect(polls()).toBe(2)
})

test('A11: a held check outlives the 10 s hook budget when the phone answers late', { timeoutMs: 30000 }, async ($, on) => {
  installBoilerplate(on, { HOME: '/home/u' })
  const asks = new Map<string, { answer: Record<string, unknown> | undefined }>()
  on('http.fetch', ($, e, next) => {
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (e.url.includes('/wait')) {
      const ask = parseWaitAsk(e.url)
      // Park 500 ms of real time: the hold re-waits on its own ask, each fetch
      // under its own hook budget, so the hold outlives the 10 s bound.
      return new Promise(resolve =>
        setTimeout(() => {
          const a = asks.get(ask)
          if (a?.answer !== undefined) {
            resolve(res(200, JSON.stringify({ remote: true, answer: a.answer, closed: true })))
          } else {
            resolve(res(200, JSON.stringify({ remote: true, answer: null, closed: false })))
          }
        }, 500),
      )
    }
    if (body !== undefined && body.kind === 'ask_opened') {
      asks.set(body.ask as string, { answer: undefined })
      return res(200, JSON.stringify({ ok: true, remote: true, hold: true }))
    }
    return res(200, JSON.stringify({ ok: true }))
  })
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const t0 = Date.now()
  let settledAt = -1
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } }).then(v => {
    settledAt = Date.now() - t0
    return v
  })
  await new Promise(r => setTimeout(() => r(undefined), 12000))
  const ask = [...asks.keys()][0]!
  asks.set(ask, { answer: { decision: 'allow' } })
  await new Promise(r => setTimeout(() => r(undefined), 1500))
  expect(await p).toEqual({ decision: 'allow' })
  expect(settledAt).toBeGreaterThan(10000)
})

// The timer owns the session's one poll, even while a check is held: a hold
// waits on its own `/wait`, never on the timer's poll, so the poll keeps
// carrying prompts and interrupts.
test('the timer polls while a hold is live', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const polls = () => bridge.fetches.filter(f => f.url.includes('/poll')).length
  await clock.advance(1000)
  expect(polls()).toBe(1) // the timer polled during the hold; the old skip is gone
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ decision: 'allow' })
})

test('R1c: a desk press settles at once while the hold wait is parked; its later reply is ignored', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  // The hold's wait is parked (mock clock); the press must settle now, not
  // after that wait returns.
  await ui.press({ key: 'allow' })
  const v = await Promise.race([
    p,
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(v).toEqual({ decision: 'allow' })
  // The abandoned wait later returns closed; the entry is gone, so nothing
  // resolves again and nothing closes twice.
  await clock.advance(PARK_MS)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
  await ui.unmount()
})

// An inline plugin that stalls an append POST 12 s in REAL time, standing for a
// wedged daemon that holds the send chain's link open.
const slowAppend = {
  name: 'slowappend',
  register(on: On) {
    on('http.fetch', async ($, e, next) => {
      const b = e.init?.body
      if (b === undefined || !b.includes('"kind":"append"')) return next(e)
      const t = Date.now()
      while (Date.now() - t < 12000) await $.http.fetch('http://inner/wait', {})
      return { value: { status: 200, ok: true, headers: {}, text: '{"ok":true}' } }
    })
  },
}

test('R2: an abort before the hold starts posts ask_closed and leaves no zombie row', { timeoutMs: 40000, plugins: [slowAppend] }, async ($, on) => {
  const { clock } = installBoilerplate(on, { HOME: '/home/u' })
  const st = { closes: 0 }
  on('http.fetch', ($, e, next) => {
    if (e.url.includes('/wait')) {
      return new Promise(resolve => setTimeout(() => resolve(res(200, '[]')), 1000))
    }
    if (e.url.includes('/poll')) {
      return new Promise(resolve => setTimeout(() => resolve(res(200, JSON.stringify({ remote: true, commands: [] }))), 1000))
    }
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined && body.kind === 'ask_opened') {
      return res(200, JSON.stringify({ ok: true, remote: true, hold: true }))
    }
    if (body !== undefined && body.kind === 'ask_closed') st.closes += 1
    return res(200, JSON.stringify({ ok: true }))
  })
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  void $.session.append(appendRow('u1'))
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await new Promise(r => setTimeout(() => r(undefined), 16000))
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  const row = await ui.find({ key: 'allow' })
  await ui.unmount()
  const v = await Promise.race([
    p,
    new Promise(resolve => setTimeout(() => resolve(undefined), 100)),
  ])
  expect(v).toEqual({ decision: 'ask' })
  expect(row).toBeUndefined()
  expect(st.closes).toBe(1)
})

// An inline plugin that settles the 'ls' check first (abandoning that hold a
// moment after it is drawn) and passes every other check through, standing for
// a hook above the mod that settles while the mod still holds.
const abortOne = {
  name: 'abortone',
  tier: 'prepend' as const,
  register(on: On) {
    on('tool.check', async ($, e, next) => {
      const cmd = (e.input as { command?: string })?.command
      if (cmd !== 'ls') return next(e)
      void next(e)
      await new Promise(resolve => setTimeout(() => resolve(undefined), 200))
      return { decision: 'allow' }
    })
  },
}

test('R3: aborting one hold resolves only its own ask; the other stays held', { timeoutMs: 15000, plugins: [abortOne] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  // The inline plugin settles the 'ls' check first, abandoning that hold; the
  // 'rm' check holds normally.
  const pA = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  const pB = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await clock.settle()
  // Wait out the inline plugin's delayed settle, which aborts A's hold.
  await new Promise(r => setTimeout(() => r(undefined), 300))
  // A's ask was closed by its own abort; B's ask is still held: the row shows
  // it and the phone can still answer it.
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  expect(await ui.find({ key: 'allow' })).toBeDefined()
  const askB = (bridge.events.find(
    e => e.kind === 'ask_opened' && (e.input as { command?: string })?.command === 'rm',
  ) as { ask: string }).ask
  bridge.answer(askB, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await pB).toEqual({ decision: 'allow' })
  expect(await pA).toEqual({ decision: 'allow' })
  await ui.unmount()
})

// An inline plugin standing for the engine above the mod. The 'ls' check settles
// 200 ms after the mod starts holding it, as the desk's Esc does; the 'rm' check
// settles only when `turn.abort` runs, as an aborted turn ends every check it is
// waiting on. Either settle aborts the mod's hold.
const engineAbort = {
  name: 'engineabort',
  tier: 'prepend' as const,
  register(on: On) {
    let endTurn: (() => void) | null = null
    on('turn.abort', ($, e, next) => {
      endTurn?.()
      return { value: undefined }
    })
    on('tool.check', async ($, e, next) => {
      void next(e)
      const cmd = (e.input as { command?: string })?.command
      await new Promise<void>(resolve => {
        if (cmd === 'ls') setTimeout(() => resolve(), 200)
        else endTurn = resolve
      })
      return { decision: 'deny' }
    })
  },
}

test('R3b: a hold its own interrupt aborted closes by interrupt; a desk abort closes by desk', { timeoutMs: 15000, plugins: [engineAbort] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })
  const closes = () => bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))

  // The desk's Esc: the engine settles the check with no interrupt behind it.
  const pDesk = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const deskAsk = bridge.lastAsk()!
  await pDesk
  await new Promise(r => setTimeout(() => r(undefined), 200))
  expect(closes()).toEqual([{ ask: deskAsk, by: 'desk' }])

  // The phone's interrupt: the mod aborts the turn, which ends the held check.
  const pPhone = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await clock.settle()
  const phoneAsk = bridge.lastAsk()!
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await pPhone
  await new Promise(r => setTimeout(() => r(undefined), 200))
  expect(closes()).toEqual([
    { ask: deskAsk, by: 'desk' },
    { ask: phoneAsk, by: 'interrupt' },
  ])
})

// An inline plugin holding every `ask_opened` post 300 ms in real time, standing
// for a send chain queued behind other posts, so an interrupt can land while an
// ask is minted but not yet held.
const slowOpen = {
  name: 'slowopen',
  register(on: On) {
    on('http.fetch', async ($, e, next) => {
      const b = e.init?.body
      if (b === undefined || !b.includes('"kind":"ask_opened"')) return next(e)
      await new Promise(resolve => setTimeout(() => resolve(undefined), 300))
      return next(e)
    })
  },
}

test('R3c: an interrupt landing while ask_opened is in flight still closes that ask by interrupt', { timeoutMs: 15000, plugins: [engineAbort, slowOpen] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await new Promise(r => setTimeout(() => r(undefined), 50))
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await p
  await new Promise(r => setTimeout(() => r(undefined), 500))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask: bridge.lastAsk(), by: 'interrupt' }])
})

// An inline plugin whose engine accepts `turn.abort` but whose abort reaches no
// held check, so the desk still answers the hold afterwards.
const abortReachesNothing = {
  name: 'abortreachesnothing',
  tier: 'prepend' as const,
  register(on: On) {
    on('turn.abort', ($, e, next) => ({ value: undefined }))
  },
}

test('R3d: a desk press after an interrupt whose abort missed the hold closes by desk', { timeoutMs: 15000, plugins: [abortReachesNothing] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask = bridge.lastAsk()!
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  const ui = await $.ui.mount({ plugin: 'clauth', surface: 'terminal', component: 'AbovePrompt', props: band(80) })
  await ui.press({ key: 'allow' })
  expect(await p).toEqual({ decision: 'allow' })
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask, by: 'desk' }])
  await ui.unmount()
})

// An inline plugin whose engine refuses `turn.abort` (it throws), and whose
// 'ls' check the desk's Esc settles 300 ms after the mod starts holding it.
const abortThrows = {
  name: 'abortthrows',
  tier: 'prepend' as const,
  register(on: On) {
    on('turn.abort', ($, e, next) => {
      throw new Error('no such turn')
    })
    on('tool.check', async ($, e, next) => {
      void next(e)
      await new Promise(resolve => setTimeout(() => resolve(undefined), 300))
      return { decision: 'deny' }
    })
  },
}

test('R3e: a desk Esc after an interrupt whose abort was refused closes by desk', { timeoutMs: 15000, plugins: [abortThrows] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask = bridge.lastAsk()!
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await p
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask, by: 'desk' }])
})

// An inline plugin slowing every `$.state.set` by 50 ms in real time, standing
// for a host round trip the aborted turn's own `turn.complete` can overtake.
const slowStateSet = {
  name: 'slowstateset',
  tier: 'prepend' as const,
  register(on: On) {
    on('state.set', async ($, e, next) => {
      await new Promise(resolve => setTimeout(() => resolve(undefined), 50))
      return next(e)
    })
  },
}

test('R3f: the interrupt label survives the aborted turn completing before the close is posted', { timeoutMs: 15000, plugins: [engineAbort, slowStateSet] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'rm' } })
  await clock.settle()
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const ask = bridge.lastAsk()!
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await p
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: true, turnId: 't1' })
  await new Promise(r => setTimeout(() => r(undefined), 300))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask, by: 'interrupt' }])
})

// An inline plugin standing for a hold that outlives its turn (a background
// subagent's check): `turn.abort` reaches no held check, and the 'ls' check
// settles 600 ms after the mod starts holding it, whatever turn is current then.
const lateAbort = {
  name: 'lateabort',
  tier: 'prepend' as const,
  register(on: On) {
    on('turn.abort', ($, e, next) => ({ value: undefined }))
    on('tool.check', async ($, e, next) => {
      void next(e)
      await new Promise(resolve => setTimeout(() => resolve(undefined), 600))
      return { decision: 'deny' }
    })
  },
}

test('R3g: a hold aborted after its interrupted turn completed closes by desk', { timeoutMs: 15000, plugins: [lateAbort] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask = bridge.lastAsk()!
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: true, turnId: 't1' })
  await p
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask, by: 'desk' }])
})

test('R3h: a hold from an earlier turn aborted during a later interrupted turn closes by desk', { timeoutMs: 15000, plugins: [lateAbort] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.turn.start({ text: 'x', turnId: 't1' })

  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const ask = bridge.lastAsk()!
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't1' })
  await $.turn.start({ text: 'y', turnId: 't2' })
  bridge.poll.commands = [{ kind: 'interrupt' }]
  await clock.advance(PARK_MS)
  await p
  await new Promise(r => setTimeout(() => r(undefined), 200))
  const closes = bridge.events.filter(b => b.kind === 'ask_closed').map(b => ({ ask: b.ask, by: b.by }))
  expect(closes).toEqual([{ ask, by: 'desk' }])
})

// An inline plugin emulating the daemon's poll replacement: each /poll bumps an
// epoch and parks; a newer /poll on the same instance wakes the older one empty
// (the N1 mechanism that made two holds pumping /poll loop at ~60 polls/3 s).
// The fix routes holds through /wait, so only the timer polls — once per tick.
const epochBridge = {
  name: 'epochbridge',
  register(on: On) {
    let epoch = 0
    let polls = 0
    on('http.fetch', async ($, e, next) => {
      if (!e.url.includes('/poll')) return next(e)
      polls += 1
      epoch += 1
      const mine = epoch
      const t = Date.now()
      for (;;) {
        const r = await $.http.fetch('http://inner/wait?n=' + String(polls), {})
        if (epoch !== mine) {
          return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify({ remote: true, commands: [] }) } }
        }
        if (r.text !== '[]' || Date.now() - t >= 18000) {
          if (r.text !== '[]') await $.http.fetch('http://inner/drain', {})
          return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify({ remote: true, commands: JSON.parse(r.text) }) } }
        }
      }
    })
  },
}

test('N1: two held asks wait on their own asks, never a poll loop', { timeoutMs: 30000, plugins: [epochBridge] }, async ($, on) => {
  installBoilerplate(on, { HOME: '/home/u' })
  const asks = new Map<string, { answer: Record<string, unknown> | undefined; cmd?: string }>()
  const st = { maxPoll: 0 }
  on('http.fetch', ($, e, next) => {
    if (e.url.includes('inner/wait')) {
      st.maxPoll = Math.max(st.maxPoll, Number(e.url.split('n=')[1] ?? '0'))
      return new Promise(resolve => setTimeout(() => resolve(res(200, '[]')), 100))
    }
    if (e.url.includes('inner/drain')) {
      return res(200, '{}')
    }
    if (e.url.includes('localhost/wait')) {
      const ask = parseWaitAsk(e.url)
      const deliver = () => {
        const a = asks.get(ask)
        if (a?.answer !== undefined) {
          return res(200, JSON.stringify({ remote: true, answer: a.answer, closed: true }))
        }
        return res(200, JSON.stringify({ remote: true, answer: null, closed: false }))
      }
      return new Promise(resolve => setTimeout(() => resolve(deliver()), 100))
    }
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined && body.kind === 'ask_opened') {
      const cmd = (body.input as { command?: string })?.command
      asks.set(body.ask as string, { answer: undefined, cmd })
      return res(200, JSON.stringify({ ok: true, remote: true, hold: true }))
    }
    return res(200, JSON.stringify({ ok: true }))
  })
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p1 = $.tool.check({ tool: 'Bash', input: { command: 'c1' } })
  const p2 = $.tool.check({ tool: 'Bash', input: { command: 'c2' } })
  await new Promise(r => setTimeout(() => r(undefined), 300))
  const byCmd = (c: string) => [...asks.entries()].find(([, v]) => v.cmd === c)![0]
  const a1 = byCmd('c1')
  const a2 = byCmd('c2')
  const before = st.maxPoll
  await new Promise(r => setTimeout(() => r(undefined), 3000))
  const pollsIn3s = st.maxPoll - before
  // The timer polls once per tick at most; two holds pumping /poll would have
  // replaced each other dozens of times in this window.
  expect(pollsIn3s).toBeLessThan(10)
  // Each hold settles from its own phone answer through its own /wait.
  asks.set(a2, { answer: { decision: 'deny' }, cmd: 'c2' })
  await expect(Promise.race([p2, new Promise(r => setTimeout(() => r('timeout'), 4000))])).resolves.toEqual({ decision: 'deny' })
  asks.set(a1, { answer: { decision: 'allow' }, cmd: 'c1' })
  await expect(Promise.race([p1, new Promise(r => setTimeout(() => r('timeout'), 4000))])).resolves.toEqual({ decision: 'allow' })
})

// A wrong-form device answer (an `answers` body for a permission ask) is a
// loss, never a re-wait: the fix must not loop `/wait` on the closed ask.
test('W1: a wrong-form answer resolves as a loss with a bounded wait count', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  await clock.settle()
  const waits = () => bridge.fetches.filter(f => f.url.includes('/wait')).length
  expect(waits()).toBe(1)
  bridge.answer(bridge.lastAsk()!, { answers: { q: 'y' } })
  await clock.advance(PARK_MS)
  await clock.advance(PARK_MS * 20)
  expect(waits()).toBe(1)
  expect(await p).toEqual({ decision: 'ask' })
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

// An empty `answers` map holds nothing the engine can use, so it is a loss
// too: the question falls back to the engine's own dialog.
test('W1b: an empty answers map resolves the question as a loss', async ($, on) => {
  const { clock, bridge } = setup(on)
  let nextRan = false
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => {
    nextRan = true
    return { result: { answers: { q: 'engine' } } }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.tool.call({ tool: 'AskUserQuestion', questions: QUESTION })
  await clock.settle()
  bridge.answer(bridge.lastAsk()!, { answers: {} })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ result: { answers: { q: 'engine' } } })
  expect(nextRan).toBe(true)
  expect(bridge.events.filter(b => b.kind === 'ask_closed')).toHaveLength(1)
})

// `session.end` awaits its `end` post before returning, so the daemon learns
// the end now instead of from its 40 s silence sweep.
test('N2: session.end awaits the end post before returning', async ($, on) => {
  installBoilerplate(on, { HOME: '/home/u' })
  let endSeen = false
  let releaseEnd: () => void = () => {}
  on('http.fetch', ($, e, next) => {
    const body: EventBody | undefined =
      e.init?.body !== undefined ? (JSON.parse(e.init.body) as EventBody) : undefined
    if (body !== undefined && body.kind === 'end') {
      endSeen = true
      return new Promise(resolve => {
        releaseEnd = () => resolve(res(200, JSON.stringify({ ok: true })))
      })
    }
    if (e.url.includes('/poll') || e.url.includes('/wait')) {
      return res(200, JSON.stringify({ remote: true, commands: [] }))
    }
    return res(200, JSON.stringify({ ok: true }))
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const p = $.session.end({ reason: 'other', sessionId: 'sid-1', resume: { id: 'sid-1' } })
  await new Promise(r => setTimeout(() => r(undefined), 100))
  expect(endSeen).toBe(true)
  let ended = false
  void p.then(() => {
    ended = true
  })
  // Flush microtasks: a `session.end` that did not await the post has already
  // resolved by now, so `ended` is true on the old `void postEvent` form.
  await new Promise(r => setTimeout(() => r(undefined), 0))
  expect(ended).toBe(false)
  releaseEnd()
  await p
  expect(ended).toBe(true)
})

// The note command runs on the call's own scope, and its envelope reaches
// the model as the result's context.
test('P1: the note command runs on the call and its envelope reaches the model', async ($, on) => {
  setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, {
    stdout: noteEnvelope(
      'clauth note: the active profile for this session switched from `cld0` to `kerry`.',
    ),
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.tool.call(toolCall())

  expect(out.context).toEqual([
    'clauth note: the active profile for this session switched from `cld0` to `kerry`.',
  ])
  expect(runs).toHaveLength(1)
  expect(runs[0]!.argv).toEqual(['clauth', 'hook-profile-changed-note'])
  expect(runs[0]!.init?.timeoutMs).toBe(10_000)
  expect(JSON.parse(runs[0]!.init?.stdin ?? '')).toEqual({
    session_id: 'sid-1',
    hook_event_name: 'PostToolUse',
    tool_name: 'Bash',
  })
})

// The context-size note rides the same subcommand and envelope, so one run can
// carry both; its text must reach the model unchanged.
test('P2: the 600k context note reaches the model unchanged', async ($, on) => {
  setup(on)
  toolBottom(on)
  const note =
    'clauth: context window usage has exceeded 600k tokens. consider writing or updating a handoff prompt so a fresh session can take over.'
  installNoteCommand(on, { stdout: noteEnvelope(note) })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.tool.call(toolCall())

  expect(out.context).toEqual([note])
})

// The whole cost being removed: the command is a subprocess, and the tool
// calls inside one cadence window must not each pay for one.
test('P3: tool calls in one cadence window share one subprocess', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: noteEnvelope('note') })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  for (let i = 0; i < 3; i += 1) await $.tool.call(toolCall())
  expect(runs).toHaveLength(1)

  await clock.advance(NOTE_CADENCE_MS - 1)
  await $.tool.call(toolCall())
  expect(runs).toHaveLength(1)

  await clock.advance(1)
  const out = await $.tool.call(toolCall())
  expect(runs).toHaveLength(2)
  expect(out.context).toEqual(['note'])
})

// The mod sits above whatever else answers the call: its note joins that
// context instead of replacing it, and the result passes through.
test('P4: the note joins the chain context, it does not replace it', async ($, on) => {
  setup(on)
  installNoteCommand(on, { stdout: noteEnvelope('note') })
  toolBottom(on, { value: { result: 'kept', context: ['beneath'] } })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.tool.call(toolCall())

  expect(out.result).toBe('kept')
  expect(out.context).toEqual(['beneath', 'note'])
})

// The command is silent in the ordinary case, and a failure is silence too: a
// broken or missing `clauth` must never break the tool result it rides on.
test('P5: a silent, failing or unparseable command adds no context', async ($, on) => {
  const { clock } = setup(on)
  const answer: { stdout?: string; exitCode?: number } = { stdout: '' }
  const runs = installNoteCommand(on, answer)
  toolBottom(on, { value: { result: 'kept' } })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  const first = await $.tool.call(toolCall())
  expect(first.result).toBe('kept')
  expect(first.context).toBeUndefined()

  await clock.advance(NOTE_CADENCE_MS)
  answer.exitCode = 1
  const second = await $.tool.call(toolCall())
  expect(second.result).toBe('kept')
  expect(second.context).toBeUndefined()

  await clock.advance(NOTE_CADENCE_MS)
  answer.exitCode = 0
  answer.stdout = 'not json'
  const third = await $.tool.call(toolCall())
  expect(third.result).toBe('kept')
  expect(third.context).toBeUndefined()
  expect(runs).toHaveLength(3)
})

// A subagent's call carries its own scope, so the command's per-scope record
// lands on the subagent's, never the main thread's.
test('P6: a subagent call feeds the command its own agent scope', async ($, on) => {
  setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall('a1'))

  expect(runs).toHaveLength(1)
  expect(JSON.parse(runs[0]!.init?.stdin ?? '')).toEqual({
    session_id: 'sid-1',
    hook_event_name: 'PostToolUse',
    tool_name: 'Bash',
    agent_id: 'a1',
  })
})

// A main-scope agent-spawn call is the one fire the headroom nudge computes
// on, and a skipped fire loses it for good, so the window never skips it; it
// also leaves the window where it stood.
test('P7: a main-scope agent spawn runs inside the window without taking it', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall())
  await clock.advance(1_000)
  await $.tool.call(spawnCall())

  expect(runs).toHaveLength(2)
  expect((JSON.parse(runs[1]!.init?.stdin ?? '') as { tool_name?: string }).tool_name).toBe('Agent')

  await clock.advance(NOTE_CADENCE_MS - 1_000)
  await $.tool.call(toolCall())
  expect(runs).toHaveLength(3)
})

// One window per scope: a subagent's calls never spend the main thread's
// window, and one subagent's never spend another's.
test('P8: the window is per scope, not per module', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall())
  await clock.advance(1_000)
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(2)

  await clock.advance(1_000)
  await $.tool.call(toolCall())
  expect(runs).toHaveLength(2)
  await $.tool.call(toolCall('a2'))
  expect(runs).toHaveLength(3)
})

// The windows belong to a session: a fresh load starts with every window
// open, so its first call is checked.
test('P9: a session start opens every window', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall())
  await $.tool.call(toolCall('a1'))
  await clock.advance(1_000)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall())
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(4)
})

// A subagent's run ending drops its window entry: nothing else bounds the
// map but the sweep, and a stale entry could only withhold a run a later
// call owes.
test('P10: a subagent run ending releases its window entry', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall('a1'))
  await clock.advance(1_000)
  await $.turn.complete(agentEnd('a1'))
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(2)
})

// Parallel tool calls settle together; the window is stamped before the run's
// first await, so a burst inside one window is still one process.
test('P11: a parallel burst inside one window runs once', async ($, on) => {
  setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await Promise.all([0, 1, 2, 3].map(() => $.tool.call(toolCall())))
  expect(runs).toHaveLength(1)
})

// A run the engine rejects (its 10 s bound, a missing `clauth`) is silence,
// and the call still settles with what the hooks beneath answered.
test('P12: a rejected run adds no context', async ($, on) => {
  setup(on)
  toolBottom(on, { value: { result: 'kept' } })
  on('process.run', async () => {
    throw new Error('spawn failed')
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.tool.call(toolCall())
  expect(out.result).toBe('kept')
  expect(out.context).toBeUndefined()
})

// PostToolUse fired for an answered call alone: a refused or errored call
// carries no note, and leaves the window open for the next answered one.
test('P13: a refused or errored call runs nothing and spends no window', async ($, on) => {
  setup(on)
  const answer: { value: ToolCallResult } = { value: { deny: 'not here' } }
  toolBottom(on, answer)
  const runs = installNoteCommand(on, { stdout: noteEnvelope('note') })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  expect((await $.tool.call(toolCall())).deny).toBe('not here')
  answer.value = { isError: true, result: 'boom', text: 'boom' }
  expect((await $.tool.call(toolCall())).isError).toBe(true)
  expect(runs).toHaveLength(0)

  answer.value = { result: 'ok' }
  expect((await $.tool.call(toolCall())).context).toEqual(['note'])
  expect(runs).toHaveLength(1)
})

// An agent-spawn call skips the cadence only on the main scope: a subagent's
// spawn is its own scope's ordinary call, so its own window still holds it.
test('P14: a subagent agent-spawn call does not bypass its window', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall('a1'))
  await clock.advance(1_000)
  await $.tool.call(spawnCall('a1'))
  expect(runs).toHaveLength(1)
})

// A `/clear` ends the conversation and no `session.start` follows it, so the
// end opens the windows: the next conversation's first calls are checked.
test('P15: a session end opens every window', async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall())
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(2)
  await clock.advance(1_000)
  await $.session.end({ reason: 'clear', sessionId: 'sid-1', resume: { id: 'sid-1' } })
  await $.tool.call(toolCall())
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(4)
})

// A fire that cannot name its session runs nothing, so it leaves the window
// open for the next call.
test('P16: a fire that cannot read its session id spends no window', async ($, on) => {
  const session = { down: false }
  setup(on, { HOME: '/home/u' }, true, session)
  toolBottom(on)
  const runs = installNoteCommand(on, { stdout: noteEnvelope('note') })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  session.down = true
  expect((await $.tool.call(toolCall())).context).toBeUndefined()
  expect(runs).toHaveLength(0)

  session.down = false
  expect((await $.tool.call(toolCall())).context).toEqual(['note'])
  expect(runs).toHaveLength(1)
})

test('Q1: a hook failing after next still answers what the hooks beneath did', async ($, on) => {
  on('clock.now', () => { throw new Error('clock down') })
  on('session.id', ($, e, next) => ({ value: 'sid-1' }))
  let bottomRuns = 0
  on('tool.call', ($, e, next) => {
    bottomRuns += 1
    return { result: 'kept' }
  })
  const runs = installNoteCommand(on, { stdout: '' })
  const out = await $.tool.call(toolCall())
  expect(out.result).toBe('kept')
  expect(out.context).toBeUndefined()
  expect(runs).toHaveLength(0)
  expect(bottomRuns).toBe(1)
})

// A plugin above the mod's hooks, logging how each link beneath it settled.
const TRACER: Plugin = {
  name: 'tracer',
  tier: 'prepend',
  register(on) {
    on('tool.call', async ($, e, next) => {
      const settled = await next(e)
      for (const link of next.trace) await $.ui.log(`${link.plugin}: ${link.outcome}`)
      return settled
    })
  },
}

// Without its own `.catch` a failure after `next` is a failed hook, which the
// engine records against the plugin; the result stands either way.
test('Q2: a failure after next lands in the note hook own catch', { plugins: [TRACER] }, async ($, on) => {
  on('clock.now', () => { throw new Error('clock down') })
  on('session.id', ($, e, next) => ({ value: 'sid-1' }))
  toolBottom(on, { value: { result: 'kept' } })
  const lines = installSkipLog(on)
  installNoteCommand(on, { stdout: '' })
  await $.tool.call(toolCall())
  expect(lines).toContain('clauth: caught')
})

// --- an Edit ask's start line (group L) ---

// How long the mod lets the pre-edit read run before the ask opens without it.
const START_LINE_READ_BOUND_MS = 1000

const EDIT_PATH = '/w/f.txt'

function editInput(oldString: unknown, replaceAll?: unknown): Record<string, unknown> {
  return {
    file_path: EDIT_PATH,
    old_string: oldString,
    new_string: 'n',
    ...(replaceAll !== undefined ? { replace_all: replaceAll } : {}),
  }
}

// The file as `$.fs.read` finds it, with every read recorded: `body` is a text,
// an Error the read rejects with, or a promise the test settles itself.
function installDisk(on: On) {
  const disk = {
    body: '' as string | Error | Promise<string>,
    reads: [] as { path: string; as: string }[],
  }
  on('fs.read', async ($, e, next) => {
    disk.reads.push({ path: e.path, as: e.as })
    const body = disk.body
    if (body instanceof Error) throw body
    return { value: await body }
  })
  return disk
}

function deferred<T>() {
  let resolve: (value: T) => void = () => {}
  const promise = new Promise<T>(r => {
    resolve = r
  })
  return { promise, resolve }
}

function opened(events: EventBody[]): EventBody[] {
  return events.filter(b => b.kind === 'ask_opened')
}

test('L1: the posted line is the 1-based line of the match in the bytes read', async ($, on) => {
  const { bridge } = setup(on, undefined, false)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const cases: [string, string, string, unknown, number | null][] = [
    ['first line', 'alpha\nbeta\ngamma\n', 'alpha', undefined, 1],
    ['last line', 'alpha\nbeta\ngamma\n', 'gamma', undefined, 3],
    ['no final newline', 'alpha\nbeta\ngamma', 'gamma', undefined, 3],
    ['past empty lines', 'a\n\n\nb\n', 'b', undefined, 4],
    ['many lines in old_string', 'a\nb\nc\nd\n', 'b\nc', undefined, 2],
    ['old_string opening on a newline', 'a\nb\n', '\nb', undefined, 1],
    ['mid-line', 'one\nsay alpha now\n', 'alpha', undefined, 2],
    ['after non-ascii text', 'é😀\nb\n', 'b', undefined, 2],
    ['crlf', 'one\r\ntwo\r\nthree\r\n', 'three', undefined, 3],
    ['crlf in old_string', 'one\r\ntwo\r\nthree\r\n', 'two\r\nthree', undefined, null],
    ['lf old_string on a crlf file', 'one\r\ntwo\r\nthree\r\n', 'two\nthree', undefined, 2],
    ['lf old_string twice on a crlf file', 'a\r\nb\r\na\r\nb\r\n', 'a\nb', undefined, null],
    ['both endings, first form first', 'a\r\nb\r\nX\na\nb\n', 'a\nb', true, 1],
    ['a folded overlap', 'x\r\nx\nx\n', 'x\nx', undefined, null],
    ['mixed endings inside one match', 'a\r\nb\nc\n', 'a\nb\nc', undefined, 1],
    ['three lines on a crlf file', 'a\r\nb\r\nc\r\nd\r\n', 'b\nc\nd', undefined, 2],
    ['a utf-16 file read as utf-8', '��\nN\r\0\n\0x\0\r\0\n\0', 'x', undefined, null],
    ['a utf-16 file with no nul', '��\nA上A\nN', '上', undefined, null],
    ['a nul in a utf-8 file', 'a\0b\nc\n', 'c', undefined, 2],
    ['one replacement character opens the file', '�a\nb\n', 'b', undefined, 2],
    ['mixed endings', 'one\ntwo\r\nthree\nfour\r\n', 'four', undefined, 4],
    ['a lone cr ends no line', 'a\rb\nc\n', 'c', undefined, 2],
    ['replace_all, one match', 'x\nA\n', 'A', true, 2],
    ['replace_all, several: the first', 'x\nA\ny\nA\n', 'A', true, 2],
    ['replace_all, overlapping: the first', 'q\nbabab\n', 'bab', true, 2],
    ['two matches, no replace_all', 'x\nA\ny\nA\n', 'A', undefined, null],
    ['two matches, replace_all false', 'x\nA\ny\nA\n', 'A', false, null],
    ['two matches, replace_all not a boolean', 'x\nA\ny\nA\n', 'A', 'true', null],
    ['overlapping matches, no replace_all', 'q\nbabab\n', 'bab', undefined, null],
    ['overlapping at the start', 'aaa\n', 'aa', undefined, null],
    ['not found', 'alpha\n', 'zeta', undefined, null],
  ]
  for (const [name, text, oldString, replaceAll, line] of cases) {
    disk.body = text
    await $.tool.check({ tool: 'Edit', input: editInput(oldString, replaceAll) } as never)
    expect([name, opened(bridge.events).at(-1)!.start_line]).toEqual([name, line])
  }
  expect(opened(bridge.events)).toHaveLength(cases.length)
  expect(disk.reads[0]).toEqual({ path: EDIT_PATH, as: 'text' })
})

test('L2: a failed read posts null and the ask opens and answers as before', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const failures = [
    new Error('$.fs.read failed: ENOENT'),
    new Error('$.fs.read: refused: the file is over the 4194304-byte limit'),
  ]
  for (const failure of failures) {
    disk.body = failure
    const p = $.tool.check({ tool: 'Edit', input: editInput('alpha') } as never)
    await clock.settle()
    expect(opened(bridge.events).at(-1)!.start_line).toBeNull()
    bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
    await clock.advance(PARK_MS)
    expect(await p).toEqual({ decision: 'allow' })
  }
  expect(opened(bridge.events)).toHaveLength(failures.length)
})

test('L3: a file changed between validation and the read posts null, and the ask holds', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const changed = ['someone rewrote the file\n', 'alpha\nalpha\n', '']
  for (const text of changed) {
    disk.body = text
    const p = $.tool.check({ tool: 'Edit', input: editInput('alpha') } as never)
    await clock.settle()
    expect(opened(bridge.events).at(-1)!.start_line).toBeNull()
    bridge.answer(bridge.lastAsk()!, { decision: 'deny' })
    await clock.advance(PARK_MS)
    expect(await p).toEqual({ decision: 'deny' })
  }
})

test('L4: an Edit with no usable field posts null and reads nothing', async ($, on) => {
  const { bridge } = setup(on, undefined, false)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  disk.body = 'alpha\n'
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const inputs: unknown[] = [
    {},
    { file_path: EDIT_PATH },
    { old_string: 'alpha' },
    { file_path: 7, old_string: 'alpha' },
    { file_path: EDIT_PATH, old_string: 7 },
    { file_path: EDIT_PATH, old_string: null },
    { file_path: EDIT_PATH, old_string: '' },
    null,
  ]
  for (const input of inputs) {
    await $.tool.check({ tool: 'Edit', input } as never)
    expect(opened(bridge.events).at(-1)!.start_line).toBeNull()
  }
  expect(opened(bridge.events)).toHaveLength(inputs.length)
  expect(disk.reads).toEqual([])
})

test('L5: no tool but Edit reads a file or posts a line', async ($, on) => {
  const { clock, bridge } = setup(on, undefined, false)
  on('tool.check', ($, e, next) => ASK)
  on('tool.call', { tool: 'AskUserQuestion' }, ($, e, next) => ({ result: 'dialog' }))
  const disk = installDisk(on)
  disk.body = 'alpha\n'
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await $.tool.check({ tool: 'Bash', input: { command: 'ls', ...editInput('alpha') } } as never)
  await $.tool.check({ tool: 'Write', input: { ...editInput('alpha'), content: 'x' } } as never)
  await $.tool.check({ tool: 'Read', input: { file_path: EDIT_PATH } })
  await $.tool.call({
    tool: 'AskUserQuestion',
    questions: [{ question: 'q?', header: 'h', options: [], multiSelect: false }],
  } as never)
  await clock.settle()
  const asks = opened(bridge.events)
  expect([...new Set(asks.map(b => b.tool))]).toEqual(['Bash', 'Write', 'Read', 'AskUserQuestion'])
  expect(asks.filter(b => 'start_line' in b)).toEqual([])
  expect(disk.reads).toEqual([])
})

test('L6: a check that opens no ask reads no file', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ({ decision: 'allow', reason: 'rule', rule: 'Edit' }))
  const disk = installDisk(on)
  disk.body = 'alpha\n'
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  await $.tool.check({ tool: 'Edit', input: editInput('alpha') } as never)
  expect(disk.reads).toEqual([])
  expect(opened(bridge.events)).toHaveLength(0)
})

test('L6b: a non-interactive session reads no file for an Edit ask', async ($, on) => {
  const { bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  disk.body = 'alpha\n'
  await $.session.start({ cwd: '/w', surface: null, isInteractive: false })
  await $.tool.check({ tool: 'Edit', input: editInput('alpha') } as never)
  expect(disk.reads).toEqual([])
  expect(opened(bridge.events)).toHaveLength(0)
})

test('L7: a read that never settles gives up at its bound, and the ask opens with null', async ($, on) => {
  const { clock, bridge } = setup(on)
  on('tool.check', ($, e, next) => ASK)
  const disk = installDisk(on)
  disk.body = new Promise<string>(() => {})
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })
  const p = $.tool.check({ tool: 'Edit', input: editInput('alpha') } as never)
  await clock.advance(START_LINE_READ_BOUND_MS - 1)
  expect(opened(bridge.events)).toHaveLength(0)
  await clock.advance(1)
  await clock.settle()
  expect(opened(bridge.events).map(b => b.start_line)).toEqual([null])
  bridge.answer(bridge.lastAsk()!, { decision: 'allow' })
  await clock.advance(PARK_MS)
  expect(await p).toEqual({ decision: 'allow' })
})

test('L8: asks post in check order, however long an earlier ask reads', async ($, on) => {
  const { clock, bridge } = setup(on, undefined, false)
  on('tool.check', ($, e, next) => ASK)
  const slow = deferred<string>()
  const disk = installDisk(on)
  await $.session.start({ cwd: '/w', surface: 'terminal', isInteractive: true })

  disk.body = slow.promise
  const first = $.tool.check({ tool: 'Edit', input: editInput('slow') } as never)
  await clock.settle()
  const second = $.tool.check({ tool: 'Bash', input: { command: 'ls' } })
  disk.body = 'x\nfast\n'
  const third = $.tool.check({ tool: 'Edit', input: editInput('fast') } as never)
  await clock.settle()
  // Neither later ask's post passes the read still running ahead of it.
  expect(opened(bridge.events)).toHaveLength(0)

  slow.resolve('a\nb\nslow\n')
  await Promise.all([first, second, third])
  const asks = opened(bridge.events)
  expect(asks.map(b => [b.tool, b.start_line])).toEqual([
    ['Edit', 3],
    ['Bash', undefined],
    ['Edit', 2],
  ])
  const seqs = asks.map(b => b.seq as number)
  expect(seqs).toEqual([...seqs].sort((a, b) => a - b))
})

// --- under the organization's policy plugin (group S) ---

// Where Claude Code seats `cc-plugin-sec-default` the session tells the
// person of every hook of theirs it passes over. A session raising a turn's
// worth of events (rows at every door, a tool call and its PostToolUse, a
// subagent's call, stop and end) must pass over none of the mod's.
test('S1: no hook of the mod is passed over by the policy plugin', { plugins: [POLICY] }, async ($, on) => {
  const { clock, bridge } = setup(on)
  toolBottom(on)
  classicBottoms(on)
  installNoteCommand(on, { stdout: '' })
  const skipped = installSkipLog(on)
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.session.append(appendRow('u1'))
  await $.session.append(reservedRow('attachment', 'r1'))
  await $.session.append(reservedRow('hook-context', 'r2'))
  await $.session.append(reservedRow('compaction', 'r3'))
  await $.session.append(reservedRow('tool-message', 'r4'))
  await $.session.append(reservedRow('note', 'r5'))
  await $.tool.call(toolCall())
  await $.classic.PostToolUse({ tool_name: 'Bash', tool_input: { command: 'ls' }, tool_response: 'ok', tool_use_id: 'tu-1' })
  await $.tool.call(toolCall('a1'))
  await $.classic.SubagentStop(subagentStop('a1'))
  await $.turn.complete(agentEnd('a1'))
  await clock.settle()

  expect(skipped).toEqual([])
  // The rows the policy plugin keeps to itself post no wake; the next row
  // the mod sees wakes the daemon's tail for them.
  expect(bridge.events.filter(b => b.kind === 'append').map(b => b.uuid)).toEqual(['u1'])
})

// The note is what the classic hook carried; with the policy plugin seated it
// must still reach the model, on the tool call itself.
test('S2: under the policy plugin the note still reaches the model', { plugins: [POLICY] }, async ($, on) => {
  setup(on)
  toolBottom(on)
  classicBottoms(on)
  const runs = installNoteCommand(on, { stdout: noteEnvelope('note') })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.tool.call(spawnCall())
  await $.classic.PostToolUse({ tool_name: 'Agent', tool_input: {}, tool_response: 'ok', tool_use_id: 'tu-1' })

  expect(out.context).toEqual(['note'])
  expect(runs).toHaveLength(1)
})

// A subagent's stop released its window through a classic hook; with the
// policy plugin seated the release must still happen.
test('S3: under the policy plugin a subagent run ending releases its window', { plugins: [POLICY] }, async ($, on) => {
  const { clock } = setup(on)
  toolBottom(on)
  classicBottoms(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.tool.call(toolCall('a1'))
  await clock.advance(1_000)
  await $.classic.SubagentStop(subagentStop('a1'))
  await $.turn.complete(agentEnd('a1'))
  await $.tool.call(toolCall('a1'))
  expect(runs).toHaveLength(2)
})
