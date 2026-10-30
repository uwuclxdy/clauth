import { test, expect, mock } from 'claude-code/testing'
import type { HttpInit, On } from 'claude-code'

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
  return { fetches, events, lastAsk: () => lastAsk, poll, wait, answer }
}

function installBoilerplate(on: On, env: Record<string, string> = { HOME: '/home/u' }) {
  const clock = mock.clock(on)
  mock.env(on, env)
  on('session.id', ($, e, next) => ({ value: 'sid-1' }))
  on('session.start', ($, e, next) => ({ cwd: e.cwd }))
  on('session.end', ($, e, next) => ({ sessionId: e.sessionId }))
  on('turn.start', ($, e, next) => ({ turnId: e.turnId }))
  on('turn.complete', ($, e, next) => ({ text: '' }))
  // The engine's own band, beneath the mod's: `$.ui.mount` needs something to
  // answer ui.render when the mod passes it on (nothing is held).
  on('ui.render', ($, e, next) => ({ type: 'Box' }))
  return { clock }
}

function setup(on: On, env: Record<string, string> = { HOME: '/home/u' }, hold = true) {
  const { clock } = installBoilerplate(on, env)
  const bridge = installBridge(on, clock, hold)
  return { clock, bridge }
}

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

// Nothing runs beneath a test's hooks, so a raised classic event settles on the
// test's own bottom; this installs one.
function classicBottom(
  on: On,
  result: { block?: string; additionalContext?: string[] } = {},
): void {
  on('classic.PostToolUse', ($, e, next) => result)
}

// A PostToolUse payload as the engine raises it, with the event's own scope.
function toolEvent(id: string, agentId?: string) {
  return {
    tool_name: 'Bash',
    tool_input: { command: 'ls' },
    tool_response: 'ok',
    tool_use_id: id,
    ...(agentId !== undefined ? { agent_id: agentId } : {}),
  }
}

// A main-scope agent-spawn call, the one PostToolUse the headroom nudge
// computes on.
function spawnEvent(id: string) {
  return { ...toolEvent(id), tool_name: 'Agent' }
}

// The bottom for a classic event other than PostToolUse.
function stopBottom(on: On): void {
  on('classic.SubagentStop', ($, e, next) => ({}))
}

const ASK = { decision: 'ask' as const }
const QUESTION = [
  { question: 'Which lib?', header: 'Lib', options: [{ label: 'date-fns', description: '' }], multiSelect: false },
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
        { type: 'Text', children: ['   Esc denies'] },
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

test('A3: hold:false returns the ask verdict, no hold, no ask_closed', async ($, on) => {
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
  expect(bridge.events.filter(e => e.kind === 'ask_closed')).toHaveLength(0)
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
    questions: [{ question: 'First line\nsecond line', header: 'H', options: [{ label: 'x', description: '' }], multiSelect: false }],
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
  expect(await p).toEqual({ result: { answers: { 'Which lib?': 'date-fns' } } })
  expect(nextRan).toBe(false)
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
  expect(await p).toEqual({ result: { answers: { 'Which lib?': 'date-fns' } } })
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

test('C3b: a refused prompt command does not drop the command behind it', async ($, on) => {
  const { clock, bridge } = setup(on)
  const submissions: string[] = []
  on('prompt.submit', ($, e, next) => {
    if (e.text === 'boom') throw new Error('submit refused')
    submissions.push(e.text)
    return { text: e.text }
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  bridge.poll.commands = [
    { kind: 'prompt', text: 'boom' },
    { kind: 'prompt', text: 'ok' },
  ]
  await clock.advance(PARK_MS)
  expect(submissions).toEqual(['ok'])
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

// The note command runs on the event's own scope, and its envelope reaches
// the model as the event's context.
test('P1: the note command runs on the event and its envelope reaches the model', async ($, on) => {
  setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, {
    stdout: noteEnvelope(
      'clauth note: the active profile for this session switched from `cld0` to `kerry`.',
    ),
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.classic.PostToolUse({
    session_id: 'sid-1',
    transcript_path: '/home/u/.claude/projects/p/x.jsonl',
    ...toolEvent('tu-1'),
  })

  expect(out.additionalContext).toEqual([
    'clauth note: the active profile for this session switched from `cld0` to `kerry`.',
  ])
  expect(runs).toHaveLength(1)
  expect(runs[0].argv).toEqual(['clauth', 'hook-profile-changed-note'])
  expect(runs[0].init?.timeoutMs).toBe(10_000)
  expect(JSON.parse(runs[0].init?.stdin ?? '')).toEqual({
    session_id: 'sid-1',
    transcript_path: '/home/u/.claude/projects/p/x.jsonl',
    hook_event_name: 'PostToolUse',
    tool_name: 'Bash',
  })
})

// The context-size note rides the same subcommand and envelope, so one run can
// carry both; its text must reach the model unchanged.
test('P2: the 600k context note reaches the model unchanged', async ($, on) => {
  setup(on)
  classicBottom(on)
  const note =
    'clauth: context window usage has exceeded 600k tokens. consider writing or updating a handoff prompt so a fresh session can take over.'
  installNoteCommand(on, { stdout: noteEnvelope(note) })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.classic.PostToolUse(toolEvent('tu-1'))

  expect(out.additionalContext).toEqual([note])
})

// The whole cost being removed: the command is a subprocess, and the tool
// events inside one cadence window must not each pay for one.
test('P3: tool calls in one cadence window share one subprocess', async ($, on) => {
  const { clock } = setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: noteEnvelope('note') })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  for (const id of ['tu-1', 'tu-2', 'tu-3']) await $.classic.PostToolUse(toolEvent(id))
  expect(runs).toHaveLength(1)

  await clock.advance(NOTE_CADENCE_MS - 1)
  await $.classic.PostToolUse(toolEvent('tu-4'))
  expect(runs).toHaveLength(1)

  await clock.advance(1)
  const out = await $.classic.PostToolUse(toolEvent('tu-5'))
  expect(runs).toHaveLength(2)
  expect(out.additionalContext).toEqual(['note'])
})

// The mod sits above whatever else answers the event: its note joins that
// context instead of replacing it.
test('P4: the note joins the chain context, it does not replace it', async ($, on) => {
  setup(on)
  installNoteCommand(on, { stdout: noteEnvelope('note') })
  classicBottom(on, { additionalContext: ['beneath'], block: 'stop' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  const out = await $.classic.PostToolUse(toolEvent('tu-1'))

  expect(out).toEqual({ block: 'stop', additionalContext: ['beneath', 'note'] })
})

// The command is silent in the ordinary case, and a failure is silence too: a
// broken or missing `clauth` must never break the tool result it rides on.
test('P5: a silent, failing or unparseable command adds no context', async ($, on) => {
  const { clock } = setup(on)
  const answer: { stdout?: string; exitCode?: number } = { stdout: '' }
  const runs = installNoteCommand(on, answer)
  classicBottom(on, { block: 'kept' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })

  expect(await $.classic.PostToolUse(toolEvent('tu-1'))).toEqual({ block: 'kept' })

  await clock.advance(NOTE_CADENCE_MS)
  answer.exitCode = 1
  expect(await $.classic.PostToolUse(toolEvent('tu-2'))).toEqual({ block: 'kept' })

  await clock.advance(NOTE_CADENCE_MS)
  answer.exitCode = 0
  answer.stdout = 'not json'
  expect(await $.classic.PostToolUse(toolEvent('tu-3'))).toEqual({ block: 'kept' })
  expect(runs).toHaveLength(3)
})

// A subagent's fire carries its own scope, so the command's per-scope record
// lands on the subagent's, never the main thread's.
test('P6: a subagent fire feeds the command its own agent scope', async ($, on) => {
  setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse({
    session_id: 'sid-1',
    transcript_path: '/home/u/.claude/projects/p/x.jsonl',
    ...toolEvent('tu-1', 'a1'),
  })

  expect(runs).toHaveLength(1)
  expect(JSON.parse(runs[0].init?.stdin ?? '')).toEqual({
    session_id: 'sid-1',
    transcript_path: '/home/u/.claude/projects/p/x.jsonl',
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
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse(toolEvent('tu-1'))
  await clock.advance(1_000)
  await $.classic.PostToolUse(spawnEvent('tu-2'))

  expect(runs).toHaveLength(2)
  expect((JSON.parse(runs[1].init?.stdin ?? '') as { tool_name?: string }).tool_name).toBe('Agent')

  await clock.advance(NOTE_CADENCE_MS - 1_000)
  await $.classic.PostToolUse(toolEvent('tu-3'))
  expect(runs).toHaveLength(3)
})

// One window per scope: a subagent's calls never spend the main thread's
// window, and one subagent's never spend another's.
test('P8: the window is per scope, not per module', async ($, on) => {
  const { clock } = setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse(toolEvent('tu-1'))
  await clock.advance(1_000)
  await $.classic.PostToolUse(toolEvent('tu-2', 'a1'))
  expect(runs).toHaveLength(2)

  await clock.advance(1_000)
  await $.classic.PostToolUse(toolEvent('tu-3'))
  expect(runs).toHaveLength(2)
  await $.classic.PostToolUse(toolEvent('tu-4', 'a2'))
  expect(runs).toHaveLength(3)
})

// The scope key carries the session too, so a fire naming another
// conversation never spends this one's window.
test('P9: the window key carries the session id', async ($, on) => {
  const { clock } = setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse({ session_id: 'sid-1', transcript_path: '/t', ...toolEvent('tu-1') })
  await clock.advance(1_000)
  await $.classic.PostToolUse({ session_id: 'sid-2', transcript_path: '/t', ...toolEvent('tu-2') })
  expect(runs).toHaveLength(2)
})

// A subagent's stop drops its window entry: nothing else bounds the map, and
// a stale entry could only withhold a run a later fire owes.
test('P10: a subagent stop releases its window entry', async ($, on) => {
  const { clock } = setup(on)
  classicBottom(on)
  stopBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse(toolEvent('tu-1', 'a1'))
  await clock.advance(1_000)
  await $.classic.SubagentStop({ agent_id: 'a1' })
  await $.classic.PostToolUse(toolEvent('tu-2', 'a1'))
  expect(runs).toHaveLength(2)
})

// Parallel tool calls settle together; the window is stamped before the run's
// first await, so a burst inside one window is still one process.
test('P11: a parallel burst inside one window runs once', async ($, on) => {
  setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await Promise.all(
    ['tu-1', 'tu-2', 'tu-3', 'tu-4'].map(id => $.classic.PostToolUse(toolEvent(id))),
  )
  expect(runs).toHaveLength(1)
})

// A run the engine rejects (its 10 s bound, a missing `clauth`) is silence,
// and the tool event still settles with what the hooks beneath answered.
test('P12: a rejected run adds no context', async ($, on) => {
  setup(on)
  classicBottom(on, { block: 'kept' })
  on('process.run', async () => {
    throw new Error('spawn failed')
  })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  expect(await $.classic.PostToolUse(toolEvent('tu-1'))).toEqual({ block: 'kept' })
})

// An agent-spawn call skips the cadence only on the main scope: a subagent's
// spawn is its own scope's ordinary fire, so its own window still holds it.
test('P14: a subagent agent-spawn call does not bypass its window', async ($, on) => {
  const { clock } = setup(on)
  classicBottom(on)
  const runs = installNoteCommand(on, { stdout: '' })
  await $.session.start({ cwd: '/home/u', surface: 'terminal', isInteractive: true })
  await $.classic.PostToolUse(toolEvent('tu-1', 'a1'))
  await clock.advance(1_000)
  await $.classic.PostToolUse({ ...toolEvent('tu-2', 'a1'), tool_name: 'Agent' })
  expect(runs).toHaveLength(1)
})

test('Q1: a hook failing before next still lets the hooks beneath answer', async ($, on) => {
  on('clock.now', () => { throw new Error('clock down') })
  classicBottom(on, { block: 'kept' })
  const runs = installNoteCommand(on, { stdout: '' })
  expect(await $.classic.PostToolUse(toolEvent('tu-1'))).toEqual({ block: 'kept' })
  expect(runs).toHaveLength(0)
})
