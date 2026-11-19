// The delegates band and pane of the clauth mod: this session's delegates read
// off clauth's job files, a delegate's own transcript, stop through clauth's
// `monitor` tool, no open a person did not ask for, finish/cancel notices,
// and finished rows kept for the session.
//
// The tests pin data and structure, never copy: job ids, accounts, states,
// one list entry per job, one notice per job, the exact `monitor` arguments.
// A job's list entry is found by the job id it is keyed by, and its state on
// the detail header's line holding its id once the entry is chosen. Where a
// control has to be found by its label, a word presence check stands in for
// it (`stop`/`cancel` for the stop action). The pane's id is whatever the mod
// hands `$.ui.open`.
import { test, expect, mock } from 'claude-code/testing'
import type { Engine, Mounted, Plugin } from 'claude-code/testing'
import type { On, ToolCallResult } from 'claude-code'

const HOME = '/home/u'
const JOBS = `${HOME}/.clauth/jobs`
const PROJECTS = `${HOME}/.claude/projects`
const OWN = 'sid-own'
const OTHER = 'sid-other'
// The host session's own working directory: no delegate below runs there, so
// a transcript path built from it instead of the job record's `cwd` misses.
const HOST_CWD = '/home/u/host'
// An epoch-ms start for the mocked clock; job times are set relative to it.
const T0 = 1_793_600_000_000
// How long the test lets the mod notice a change in the jobs dir: any poll
// cadence up to a minute sees it.
const POLL_MS = 60_000
const DELEGATE = 'mcp__plugin_clauth_clauth__delegate'
// The clauth MCP server as `$.mcp.call` may name it: as /mcp lists the
// plugin's server, its tool-name spelling, and a bare `clauth` server.
const CLAUTH_SERVERS: readonly string[] = ['plugin:clauth:clauth', 'plugin_clauth_clauth', 'clauth']
// A control the stop action is drawn as: a word presence check, not copy.
const STOP_WORD = /\b(stop|cancel)\b/i
// A finished row's state: the record's own word (`JobPhase::label` in
// src/mcp/jobs.rs names it `done`) or its plain-English twin.
const FINISHED_WORD = /\b(done|finished|delivered)\b/i
const RUNNING_WORD = /\brunning\b/i

const PANE_PROPS = {
  isFocused: true,
  bodyColumns: 110,
  placement: 'dock' as const,
  scroll: { offset: 0, bodyRows: 40 },
  view: {},
}

// --- clauth's job records, with the keys `JobRecord` (src/mcp/jobs.rs) writes ---

type Job = {
  id: string
  profile: string
  state: string
  startedAt: number
  hostSession?: string
  sessionId?: string
  cwd?: string
  lastOutputAt?: number
  doneAt?: number
  tail?: string
  cancelled?: boolean
  /** The host exit's or the sweep's stop marker. */
  stopped?: boolean
  /** The sweep's marker for the server that died without finishing. */
  crashed?: boolean
}

// One `<id>.json` as clauth serializes it: field order and skip-if-default
// as the struct's serde attributes have them.
function jobFile(j: Job): string {
  const rec: Record<string, unknown> = {
    job_id: j.id,
    profile: j.profile,
    state: j.state,
    started_at: j.startedAt,
  }
  // The sweep's tombstone is the one finished shape with no envelope: it
  // carries the crash marker and the stop marker instead.
  if (j.state !== 'running' && !(j.stopped === true && j.crashed === true)) {
    rec.envelope = {
      profile: j.profile,
      session_id: j.sessionId ?? null,
      result: j.cancelled === true ? '' : 'probe finished',
      ...(j.cancelled === true
        ? { cancelled: true, elapsed_secs: Math.floor(((j.doneAt ?? j.startedAt) - j.startedAt) / 1000) }
        : {}),
    }
  }
  rec.endpoint = 'api.deepseek.com'
  rec.provider = 'DeepSeek'
  rec.isolated = false
  if (j.cwd !== undefined) rec.cwd = j.cwd
  rec.spawned_by = 'cld0'
  if (j.hostSession !== undefined) rec.host_session = j.hostSession
  rec.host_pid = 4242
  if (j.sessionId !== undefined) rec.session_id = j.sessionId
  if (j.lastOutputAt !== undefined) rec.last_output_at = j.lastOutputAt
  rec.recorded_at = j.startedAt
  if (j.tail !== undefined && j.tail !== '') rec.tail = j.tail
  if (j.doneAt !== undefined) rec.done_at = j.doneAt
  rec.crashed = j.crashed ?? false
  if (j.stopped === true) rec.stopped = true
  rec.owner_pid = 5151
  rec.owner_started_at = j.startedAt - 60_000
  return JSON.stringify(rec)
}

// The delivery ledger at `<id>.json.delivered` as an older build wrote it: job
// id, claimant, instant and profile, with neither the `failed` verdict nor the
// `stopped` marker a newer writer adds beside them (`DeliveryLedger` in
// src/mcp/jobs.rs), so these fixtures read the serde defaults.
function ledgerFile(id: string, profile: string, at: number): string {
  return JSON.stringify({ job_id: id, by: 'hook', at, profile })
}

// The delivery ledger a newer build writes: the old fields, the claim's
// failure verdict, and the stop marker a record collected after its session's
// exit leaves behind.
function stoppedLedgerFile(id: string, profile: string, at: number): string {
  return JSON.stringify({ job_id: id, by: 'monitor', at, profile, failed: false, stopped: true })
}

// Claude Code's project folder for a working directory: every character
// outside [a-zA-Z0-9] becomes `-` (measured 2026-11-02).
function transcriptPath(cwd: string, sessionId: string): string {
  return `${PROJECTS}/${cwd.replace(/[^a-zA-Z0-9]/g, '-')}/${sessionId}.jsonl`
}

// A delegate transcript long enough that only its tail holds the last user
// and assistant lines: `<tag>-user-final` and `<tag>-reply-final`.
function transcriptFile(tag: string, sessionId: string, cwd: string): string {
  const base = { sessionId, cwd, version: '2.1.322', isSidechain: false }
  const lines: string[] = []
  let n = 0
  const row = (o: Record<string, unknown>) => {
    n += 1
    lines.push(JSON.stringify({ ...o, ...base, uuid: `${tag}-u${n}`, timestamp: new Date(T0 - 900_000 + n * 1000).toISOString() }))
  }
  row({ type: 'user', message: { role: 'user', content: `${tag}-user-first` } })
  for (let i = 0; i < 200; i += 1) {
    row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id: `toolu_${tag}${i}`, name: 'Bash', input: { command: `echo ${i}` } }] } })
    row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}${i}`, content: `${i}` }] } })
    row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text: `${tag}-filler-${i}` }] } })
  }
  row({ type: 'user', message: { role: 'user', content: `${tag}-user-final` } })
  row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text: `${tag}-reply-final` }] } })
  row({ type: 'system', subtype: 'turn_duration', durationMs: 1200 })
  return `${lines.join('\n')}\n`
}

// --- the world beneath the mod ---

type Notice = { via: 'toast' | 'notify'; text: string }
type McpCall = { server: string; tool: string; args: Record<string, unknown>; answered: boolean }

function b64(text: string): string {
  const bytes = new TextEncoder().encode(text)
  const abc = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
  let out = ''
  for (let i = 0; i < bytes.length; i += 3) {
    const [a, b, c] = [bytes[i]!, bytes[i + 1], bytes[i + 2]]
    out += abc[a >> 2]! + abc[((a & 3) << 4) | ((b ?? 0) >> 4)]!
    out += b === undefined ? '=' : abc[((b & 15) << 2) | ((c ?? 0) >> 6)]!
    out += c === undefined ? '=' : abc[c & 63]!
  }
  return out
}

const norm = (p: string): string => p.replace(/\/+/g, '/').replace(/(.)\/$/, '$1')

function processResult(exitCode: number, stdout: string, stderr = '') {
  return { value: { exitCode, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false } }
}

// The last `n` lines of a file's text, as `tail -n` prints them.
function tailLines(text: string, n: number): string {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text
  const lines = body.split('\n')
  return `${lines.slice(Math.max(0, lines.length - n)).join('\n')}\n`
}

// Everything beneath the plugins: a clock, HOME, the session's id, a file
// system holding only what the test writes, the host commands a reader may
// run on it, the clauth MCP server, the pane and notice surfaces, and the
// bridge socket the shipped mod already talks to. Nothing reaches the box.
function installWorld(on: On) {
  const clock = mock.clock(on, { now: T0 })
  mock.env(on, { HOME, OS: 'Linux' })
  mock.store(on)
  // The surfaces the session draws on; one terminal unless a test says more.
  const surfaces: string[] = ['terminal']
  on('session.surfaces', () => ({ value: [...surfaces] }))
  on('session.id', () => ({ value: OWN }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.end', ($, e) => ({ sessionId: e.sessionId }))
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  on('ui.render', () => ({ type: 'Box' }))

  // The shipped mod's bridge: an idle daemon, so its poll and posts settle.
  on('http.fetch', ($, e) => ({
    value: {
      status: 200,
      ok: true,
      headers: {},
      text: e.url.includes('/poll') || e.url.includes('/wait')
        ? JSON.stringify({ remote: false, commands: [], answer: null, closed: true })
        : JSON.stringify({ ok: true }),
    },
  }))

  const files = new Map<string, { text: string; mtimeMs: number }>()
  const put = (path: string, text: string) => files.set(norm(path), { text, mtimeMs: clock.now() })
  const remove = (path: string) => files.delete(norm(path))
  const isDir = (path: string) => {
    const dir = `${norm(path)}/`
    for (const p of files.keys()) if (p.startsWith(dir)) return true
    return false
  }
  on('fs.read', ($, e) => {
    const f = files.get(norm(e.path))
    if (f === undefined) throw new Error(`ENOENT: no such file or directory, open '${e.path}'`)
    return { value: e.as === 'bytes' ? { base64: b64(f.text) } : f.text }
  })
  on('fs.exists', ($, e) => ({ value: files.has(norm(e.path)) || isDir(e.path) }))
  on('fs.stat', ($, e) => {
    const f = files.get(norm(e.path))
    if (f !== undefined) {
      return { value: { kind: 'file' as const, size: new TextEncoder().encode(f.text).length, mtimeMs: f.mtimeMs, isLink: false, ...(e.resolve ? { realPath: norm(e.path) } : {}) } }
    }
    if (isDir(e.path)) return { value: { kind: 'dir' as const, size: 0, mtimeMs: T0, isLink: false, ...(e.resolve ? { realPath: norm(e.path) } : {}) } }
    throw new Error(`ENOENT: no such file or directory, stat '${e.path}'`)
  })
  on('fs.list', ($, e) => {
    const dir = `${norm(e.path)}/`
    if (!isDir(e.path)) throw new Error(`ENOENT: no such file or directory, scandir '${e.path}'`)
    const seen = new Map<string, { name: string; kind: 'file' | 'dir'; size: number; mtimeMs: number; isLink: boolean }>()
    for (const [p, f] of files) {
      if (!p.startsWith(dir)) continue
      const rest = p.slice(dir.length)
      const slash = rest.indexOf('/')
      if (slash === -1) seen.set(rest, { name: rest, kind: 'file', size: f.text.length, mtimeMs: f.mtimeMs, isLink: false })
      else seen.set(rest.slice(0, slash), { name: rest.slice(0, slash), kind: 'dir', size: 0, mtimeMs: T0, isLink: false })
    }
    return { value: [...seen.values()] }
  })
  on('fs.write', ($, e) => {
    put(e.path, e.text)
    return { value: undefined }
  })

  // Host commands: the shipped mod's note command, and the readers a pane may
  // run over a transcript (`tail`, `head`, `cat`), answered from the files above.
  on('process.run', ($, e) => {
    const argv = e.argv
    if (argv[0] === 'clauth') return processResult(0, '')
    if (argv[0] === 'head') {
      const path = argv[argv.length - 1] ?? ''
      const f = files.get(norm(path))
      if (f === undefined) return processResult(1, '', `head: cannot open '${path}' for reading: No such file or directory`)
      const bytesAt = argv.indexOf('-c')
      if (bytesAt !== -1 && argv[bytesAt + 1] !== undefined) {
        const head = new TextEncoder().encode(f.text).slice(0, Number(argv[bytesAt + 1]))
        return processResult(0, new TextDecoder().decode(head))
      }
      const linesAt = argv.indexOf('-n')
      const n = linesAt !== -1 && argv[linesAt + 1] !== undefined ? Number(argv[linesAt + 1]) : 10
      return processResult(0, f.text.split('\n').slice(0, n).map(line => `${line}\n`).join(''))
    }
    if (argv[0] === 'tail' || argv[0] === 'cat') {
      const path = argv[argv.length - 1] ?? ''
      const f = files.get(norm(path))
      if (f === undefined) return processResult(1, '', `${argv[0]}: ${path}: No such file or directory`)
      if (argv[0] === 'cat') return processResult(0, f.text)
      const bytesAt = argv.indexOf('-c')
      if (bytesAt !== -1 && argv[bytesAt + 1] !== undefined) {
        const bytes = new TextEncoder().encode(f.text)
        return processResult(0, new TextDecoder().decode(bytes.slice(Math.max(0, bytes.length - Number(argv[bytesAt + 1])))))
      }
      let n = 10
      for (let i = 1; i < argv.length - 1; i += 1) {
        const a = argv[i]!
        if (a === '-n' && argv[i + 1] !== undefined) n = Number(argv[i + 1])
        else if (/^-n\d+$/.test(a)) n = Number(a.slice(2))
        else if (a.startsWith('--lines=')) n = Number(a.slice('--lines='.length))
      }
      return processResult(0, tailLines(f.text, Number.isFinite(n) ? n : 10))
    }
    return processResult(127, '', `${argv[0]}: command not found`)
  })

  // The clauth MCP server: `monitor` answers on any of its spellings, in the
  // shape clauth gives that id. A blocking run has no collectable record, so
  // its cancel reply is an error carrying the ask's note; a done record is
  // claimed, which consumes it and hands its result to this call. Any other
  // server is not connected.
  const mcpCalls: McpCall[] = []
  const blockingIds = new Set<string>()
  const claim = (id: string, profile: string) => {
    remove(`${JOBS}/${id}.json`)
    put(`${JOBS}/${id}.json.delivered`, JSON.stringify({ job_id: id, by: 'monitor', at: clock.now(), profile }))
  }
  on('mcp.call', ($, e) => {
    const known = CLAUTH_SERVERS.includes(e.server)
    mcpCalls.push({ server: e.server, tool: e.tool, args: e.args, answered: known })
    if (!known) throw new Error(`no MCP server named "${e.server}"`)
    const id = Array.isArray(e.args.job_ids) ? String((e.args.job_ids as unknown[])[0] ?? '') : ''
    if (e.tool === 'monitor' && blockingIds.has(id)) {
      return {
        value: {
          content: [
            {
              type: 'text' as const,
              text: `asked \`${id}\` to stop; each hands back whatever it had produced.\njob_id ${id} names a blocking \`delegate\` that is still running: its result goes back through the call that started it, so there is nothing here for \`monitor\` to collect`,
            },
          ],
          isError: true,
        },
      }
    }
    if (e.tool === 'monitor' && id !== '') {
      const record = files.get(norm(`${JOBS}/${id}.json`))
      if (record !== undefined) {
        try {
          const parsed = JSON.parse(record.text) as { state?: unknown; profile?: unknown }
          if (parsed.state === 'done') {
            claim(id, typeof parsed.profile === 'string' ? parsed.profile : '')
            return { value: { content: [{ type: 'text' as const, text: 'probe finished' }], isError: false } }
          }
        } catch {
          // A record mid-write is not one a claim can read: answer as before.
        }
      }
    }
    return { value: { content: [{ type: 'text' as const, text: 'cancel requested' }], isError: false } }
  })

  // Panes, as the engine keeps them: opened, closed, listed.
  const opens: { id: string; title?: string }[] = []
  const open = new Map<string, string>()
  on('ui.open', ($, e) => {
    opens.push({ id: e.id, title: e.title })
    open.set(e.id, e.title ?? e.id)
    return { value: { isPlaced: true as const } }
  })
  on('ui.close', ($, e) => {
    open.delete(e.id)
    return { value: undefined }
  })
  on('ui.panes', () => ({
    value: [...open].map(([id, title]) => ({ id, title, isShown: true, isFocused: false, isPlaced: true })),
  }))

  // Notices: every toast and native notification the mod raises.
  const notices: Notice[] = []
  on('ui.toast', ($, e) => {
    notices.push({ via: 'toast', text: e.text })
    return { value: undefined }
  })
  on('ui.notify', ($, e) => {
    notices.push({ via: 'notify', text: `${e.title ?? ''} ${e.text}` })
    return { value: { isSent: true as const, channel: 'terminal_bell' as const } }
  })
  on('ui.status', () => ({ value: undefined }))
  on('ui.log', () => ({ value: undefined }))
  on('ui.notice', () => ({ value: undefined }))
  on('ui.copy', () => ({ value: { isCopied: true as const } }))
  on('command.register', ($, e) => ({ value: { command: e.name } }))
  on('agent.list', () => ({ value: [] }))

  // Every tool settles on this bottom; a `delegate` call answers as clauth's
  // background spawn does once its job file is written.
  on('tool.call', ($, e): ToolCallResult => {
    if (e.tool === DELEGATE) {
      return { result: [{ type: 'text', text: 'delegate started in the background' }] } as ToolCallResult
    }
    return { result: 'ok' } as ToolCallResult
  })

  return {
    clock,
    files,
    put,
    remove,
    opens,
    open,
    notices,
    mcpCalls,
    blockingIds,
    setSurfaces: (next: readonly string[]) => {
      surfaces.splice(0, surfaces.length, ...next)
    },
    job: (j: Job) => put(`${JOBS}/${j.id}.json`, jobFile(j)),
    // clauth's collect: the record goes and the delivery ledger stays.
    deliver: (id: string, profile: string) => {
      remove(`${JOBS}/${id}.json`)
      put(`${JOBS}/${id}.json.delivered`, ledgerFile(id, profile, clock.now()))
    },
  }
}

type World = ReturnType<typeof installWorld>

// --- the session as a test plays it ---

async function startSession($: Engine): Promise<void> {
  await $.session.start({ cwd: HOST_CWD, surface: 'terminal', isInteractive: true })
  // The terminal reports its layout on every drawing, so the session draws
  // once here, before a test's delegate call: an open the mod makes on its own
  // reads the fact the engine would already have handed it.
  const hint = await $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'PromptHint',
    props: { isDraft: false, isWorking: true, hint: '' },
    viewport: { columns: 200, rows: 50, isFullscreen: true },
  })
  await hint.redraw()
  await hint.unmount()
}

// The model calls clauth's `delegate` tool: clauth writes the job's file
// before the call settles, as its background spawn does.
async function delegate($: Engine, world: World, job: Job): Promise<void> {
  world.job(job)
  await $.tool.call({ tool: DELEGATE, profile: job.profile, prompt: 'run the probe', background: true })
}

// --- the pane as drawn: a tree walk over `ui.drawn()` ---

type Drawn = { type: string; props?: Record<string, unknown>; children?: unknown[] }

const isDrawn = (x: unknown): x is Drawn =>
  typeof x === 'object' && x !== null && typeof (x as Drawn).type === 'string'

// Every text an element and its descendants show: string children, and the
// props a leaf draws as text (a Button's label, a Markdown's text).
function textOf(x: unknown): string {
  if (typeof x === 'string') return x
  if (typeof x === 'number') return String(x)
  if (!isDrawn(x)) return ''
  const parts: string[] = []
  for (const prop of ['label', 'text', 'source', 'alt']) {
    const value = x.props?.[prop]
    if (typeof value === 'string') parts.push(value)
  }
  for (const child of x.children ?? []) parts.push(textOf(child))
  return parts.filter(p => p !== '').join(' ')
}

// The tree's elements, outermost first, in document order.
function elements(root: unknown): Drawn[] {
  if (!isDrawn(root)) return []
  return [root, ...(root.children ?? []).flatMap(elements)]
}

const escapeRe = (s: string): string => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

// A whole-token match: a job id or a fixture word, never a part of a longer one.
function hasWord(text: string, word: string): boolean {
  return new RegExp(`(?<![A-Za-z0-9_-])${escapeRe(word)}(?![A-Za-z0-9_-])`).test(text)
}

const buttonsIn = (root: unknown): Drawn[] => elements(root).filter(el => el.type === 'Button')

// One job's entry in the pane's list: the Button keyed by its job id.
function entryFor(root: unknown, id: string): Drawn | undefined {
  return buttonsIn(root).find(button => button.props?.key === id)
}

// Every state word a delegate's record reads as (`ClauthDelegateWord`).
const STATE_WORDS: readonly string[] = ['running', 'blocking', 'done', 'cancelled', 'stopped', 'orphaned', 'ended']

// The detail header's line naming the chosen job: the innermost element whose
// text holds its id and a state word.
function headerLine(root: unknown, id: string): Drawn | undefined {
  return elements(root)
    .filter(el => {
      const text = textOf(el)
      return hasWord(text, id) && STATE_WORDS.some(word => hasWord(text, word))
    })
    .at(-1)
}

// The key a press finds a Button by: its own, else its label.
function keyOf(button: Drawn): string {
  return String(button.props?.key ?? button.props?.label ?? '')
}

const UNIT_SECONDS: Record<string, number> = { h: 3600, min: 60, m: 60, sec: 1, s: 1 }

// Every duration a text shows, in seconds: `25m00s`, `1h06m40s`, `1500s`,
// `25 min`, `25:00`, `1:06:40`. A row's elapsed time is one of them.
function durationsIn(text: string): number[] {
  const found: number[] = []
  for (const m of text.matchAll(/(?<![\d:])(\d+):(\d{2})(?::(\d{2}))?(?![\d:])/g)) {
    const [a, b, c] = [Number(m[1]), Number(m[2]), m[3]]
    found.push(c === undefined ? a * 60 + b : a * 3600 + b * 60 + Number(c))
  }
  let run: number | null = null
  let runEnd = -1
  let lastUnit = Infinity
  for (const m of text.matchAll(/(?<![0-9_\-A-Za-gi-lo-rt-z])(\d+)\s?(h|min|m|sec|s)(?![A-Za-z])/g)) {
    const unit = UNIT_SECONDS[m[2]!]!
    const start = m.index ?? 0
    const joins = run !== null && /^\s*$/.test(text.slice(runEnd, start)) && unit < lastUnit
    if (joins) run = (run ?? 0) + Number(m[1]) * unit
    else {
      if (run !== null) found.push(run)
      run = Number(m[1]) * unit
    }
    runEnd = start + m[0].length
    lastUnit = unit
  }
  if (run !== null) found.push(run)
  return found
}

const clip = (s: string): string => (s.length > 240 ? `${s.slice(0, 240)}…` : s)

type Pane = Mounted<'terminal', 'Pane'>

// A delegate call opens no pane by itself, so with no open recorded yet the
// person asks for the delegates pane with `/delegates`.
async function askForPane($: Engine, world: World): Promise<void> {
  if (world.opens.length > 0) return
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
}

// The delegates pane, mounted where a docked pane draws. The id is the one
// the mod handed `$.ui.open`.
async function mountPane($: Engine, world: World): Promise<Pane> {
  await askForPane($, world)
  const opened = world.opens.at(-1)
  expect(opened !== undefined, 'the mod opened no pane, not even for `/delegates`').toBe(true)
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'Pane',
    requestId: opened!.id,
    props: { ...PANE_PROPS, title: opened!.title ?? opened!.id },
  })
}

// The pane redrawn from the mod's current state, as the surface draws it.
async function drawn(pane: Pane): Promise<unknown> {
  await pane.redraw()
  return pane.drawn()
}

// Chooses a job's list entry, then lets the press's own write land. The clock
// does not move, so no scan runs that the test did not ask for.
async function selectEntry(pane: Pane, world: World, id: string): Promise<void> {
  const entry = entryFor(await drawn(pane), id)
  expect(entry !== undefined, `the pane's list holds no entry keyed ${id}`).toBe(true)
  await pane.press({ key: keyOf(entry!) })
  await world.clock.settle()
}

// Chooses a job's list entry and lets any transcript read the choice started
// finish, with the pane undrawn through that window.
async function openEntry($: Engine, world: World, pane: Pane, id: string): Promise<Pane> {
  await selectEntry(pane, world, id)
  return advanceUndrawn($, world, pane)
}

// The chosen job's state line in the detail header, as the pane draws it now.
async function headerText(pane: Pane, world: World, id: string): Promise<string> {
  await selectEntry(pane, world, id)
  const tree = await drawn(pane)
  const line = headerLine(tree, id)
  expect(line !== undefined, `the detail header of ${id} has no line holding its id and a state word: ${clip(textOf(tree))}`).toBe(true)
  return textOf(line)
}

// Whether the detail header of the chosen job offers the stop action.
async function offersStop(pane: Pane, world: World, id: string): Promise<boolean> {
  await selectEntry(pane, world, id)
  return buttonsIn(await drawn(pane)).some(b => STOP_WORD.test(textOf(b)))
}

// Presses the stop action in the detail header of a running job.
async function pressStop(pane: Pane, world: World, id: string): Promise<void> {
  await selectEntry(pane, world, id)
  const tree = await drawn(pane)
  expect(headerLine(tree, id) !== undefined, `choosing ${id} draws no detail header naming it: ${clip(textOf(tree))}`).toBe(true)
  const stop = buttonsIn(tree).find(b => STOP_WORD.test(textOf(b)))
  expect(stop !== undefined, `the detail header of running ${id} offers no stop action`).toBe(true)
  await pane.press({ key: keyOf(stop!) })
  await world.clock.settle()
}

// That a job is not listed: no entry keyed by it, and its id nowhere drawn.
function expectUnlisted(root: unknown, id: string, message: string): void {
  expect(entryFor(root, id) === undefined && !hasWord(textOf(root), id), `${message}: ${clip(textOf(root))}`).toBe(true)
}

// How many notices (toast or native notification) name the job so far.
function noticesNaming(world: World, id: string): number {
  return world.notices.filter(n => hasWord(n.text, id)).length
}

// The checks one job of the list carries: its entry's account and elapsed
// time, read against the clock, and its state word on the detail header once
// the entry is chosen. A running job's elapsed time is the time since
// `started_at`, at most one poll stale; a finished one's lies between its
// run's length and the time since it started.
async function expectEntry(pane: Pane, world: World, job: Job): Promise<void> {
  const now = world.clock.now()
  const entry = entryFor(await drawn(pane), job.id)
  expect(entry !== undefined, `the pane's list holds no entry keyed by this session's ${job.id}`).toBe(true)
  const text = textOf(entry)
  expect(hasWord(text, job.profile), `the entry for ${job.id} does not name its account ${job.profile}: ${clip(text)}`).toBe(true)
  const sinceStart = Math.floor((now - job.startedAt) / 1000)
  const pollSecs = POLL_MS / 1000
  const state = await headerText(pane, world, job.id)
  if (job.state === 'running') {
    expect(RUNNING_WORD.test(state), `the header for running ${job.id} does not say running: ${clip(state)}`).toBe(true)
    const fits = durationsIn(text).some(s => s >= sinceStart - pollSecs - 1 && s <= sinceStart + 1)
    expect(fits, `the entry for ${job.id} shows no elapsed time near ${sinceStart}s: ${clip(text)}`).toBe(true)
  } else {
    expect(FINISHED_WORD.test(state), `the header for finished ${job.id} does not say done: ${clip(state)}`).toBe(true)
    expect(RUNNING_WORD.test(state), `the header for finished ${job.id} still says running: ${clip(state)}`).toBe(false)
    const ran = Math.floor(((job.doneAt ?? now) - job.startedAt) / 1000)
    const fits = durationsIn(text).some(s => s >= ran - pollSecs - 1 && s <= sinceStart + 1)
    expect(fits, `the entry for ${job.id} shows no elapsed time between ${ran}s and ${sinceStart}s: ${clip(text)}`).toBe(true)
  }
}

// A job of this session, started `agoMs` before `now`.
function ownJob(now: number, id: string, profile: string, agoMs: number, extra: Partial<Job> = {}): Job {
  return {
    id,
    profile,
    state: 'running',
    startedAt: now - agoMs,
    hostSession: OWN,
    sessionId: `sess-${id}`,
    cwd: '/work/probe',
    lastOutputAt: now - 2_000,
    // Names neither the account nor a state, so a row's account and state
    // checks cannot pass off the tail.
    tail: 'reading the build config',
    ...extra,
  }
}

// --- 1. the pane, scoped to this session ---

test('M3a-1: the pane lists exactly this session\'s delegates, each row with its id, account, state and elapsed time', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const own: Job[] = [
    ownJob(t, 'd-mvzf0a1b-0', 'D1', 1_500_000),
    ownJob(t, 'd-mvzf0a1c-1', 'cld2', 4_000_000),
    ownJob(t, 'd-mvzf0a1d-2', 'glm1', 900_000, { state: 'done', doneAt: t - 600_000 }),
  ]
  const foreign: Job[] = [
    { ...ownJob(t, 'd-mvze9x1a-0', 'D3', 1_200_000), hostSession: OTHER },
    // A record an older server wrote: no host_session at all.
    { ...ownJob(t, 'd-mvze9x1b-1', 'D4', 2_000_000), hostSession: undefined },
  ]
  // Another session's collected job: its ledger names no session.
  const foreignDelivered = 'd-mvze9x1c-2'
  for (const job of foreign) world.job(job)
  world.put(`${JOBS}/${foreignDelivered}.json.delivered`, ledgerFile(foreignDelivered, 'D5', t - 100_000))
  for (const job of own) await delegate($, world, job)
  await world.clock.advance(POLL_MS)

  const pane = await mountPane($, world)
  for (const job of own) await expectEntry(pane, world, job)
  const tree = await drawn(pane)
  for (const id of [...foreign.map(j => j.id), foreignDelivered]) {
    expectUnlisted(tree, id, `the pane lists ${id}, a delegate of another session`)
  }
  await pane.unmount()
})

// --- 2. the person closing a pane ---

// A test's `$` raises no `ui.close`, so this plugin stands in for the person
// closing every open pane: a turn whose prompt reads `close the panes` makes
// it close each one through the chain, which the mod hears as any close.
const PANE_CLOSER: Plugin = {
  name: 'pane-closer',
  register(on) {
    on('turn.start', async ($, e, next) => {
      if (e.text === 'close the panes') {
        for (const pane of await $.ui.panes()) await $.ui.close({ id: pane.id })
      }
      return next(e)
    })
  },
}

// --- 3. the transcript view ---

test('M3a-3: choosing an entry shows the tail of that delegate\'s own transcript, and the list stays drawn beside it', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  // `_`, `.` and a space in the cwd: the slug turns each into `-`.
  const alpha = ownJob(t, 'd-mvzf3aaa-0', 'D1', 300_000, { sessionId: 'sess-alpha', cwd: '/work/my_repo.v2' })
  const beta = ownJob(t, 'd-mvzf3bbb-1', 'D2', 200_000, { sessionId: 'sess-beta', cwd: '/work/beta proj' })
  world.put(transcriptPath(alpha.cwd!, alpha.sessionId!), transcriptFile('alpha', alpha.sessionId!, alpha.cwd!))
  world.put(transcriptPath(beta.cwd!, beta.sessionId!), transcriptFile('beta', beta.sessionId!, beta.cwd!))
  // The same session id under the folders a slug of `/` alone, or the host
  // session's own cwd, would name: neither is the delegate's transcript.
  world.put(`${PROJECTS}/-work-my_repo.v2/sess-alpha.jsonl`, transcriptFile('slugdecoy', 'sess-alpha', alpha.cwd!))
  world.put(transcriptPath(HOST_CWD, 'sess-alpha'), transcriptFile('hostdecoy', 'sess-alpha', HOST_CWD))
  await delegate($, world, alpha)
  await delegate($, world, beta)
  await world.clock.advance(POLL_MS)
  const ids = [alpha.id, beta.id]
  let pane = await mountPane($, world)

  pane = await openEntry($, world, pane, alpha.id)
  const tree = await drawn(pane)
  let text = textOf(tree)
  expect(hasWord(text, 'alpha-user-final'), `the view of ${alpha.id} lacks its transcript's last user line: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), `the view of ${alpha.id} lacks its transcript's last assistant line: ${clip(text)}`).toBe(true)
  for (const word of ['beta-user-final', 'beta-reply-final', 'slugdecoy-reply-final', 'hostdecoy-reply-final']) {
    expect(hasWord(text, word), `the view of ${alpha.id} shows ${word}, which is not its transcript's`).toBe(false)
  }
  for (const id of ids) {
    expect(entryFor(tree, id) !== undefined, `the view of ${alpha.id} left the list without the entry for ${id}`).toBe(true)
  }

  pane = await openEntry($, world, pane, beta.id)
  text = textOf(await drawn(pane))
  expect(hasWord(text, 'beta-user-final'), `the view of ${beta.id} lacks its transcript's last user line: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'beta-reply-final'), `the view of ${beta.id} lacks its transcript's last assistant line: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), `the view of ${beta.id} still shows ${alpha.id}'s transcript`).toBe(false)
  await pane.unmount()
})

// --- 4. stop ---

test('M3a-4: the stop action on a running row calls clauth\'s monitor with exactly that job id and cancel', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const first = ownJob(t, 'd-mvzf4aaa-0', 'D1', 120_000)
  const second = ownJob(t, 'd-mvzf4bbb-1', 'cld2', 60_000)
  const finished = ownJob(t, 'd-mvzf4ccc-2', 'glm1', 600_000, { state: 'done', doneAt: t - 300_000 })
  for (const job of [first, second, finished]) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const monitorArgs = () => world.mcpCalls.filter(c => c.tool === 'monitor').map(c => c.args)
  const answered = () => world.mcpCalls.filter(c => c.tool === 'monitor' && c.answered).map(c => c.args)
  const pane = await mountPane($, world)

  await pressStop(pane, world, first.id)
  expect(answered(), `stopping ${first.id} did not call clauth's monitor exactly once, for it alone`).toEqual([
    { job_ids: [first.id], cancel: true },
  ])
  for (const args of monitorArgs()) {
    expect(args, `a monitor call while stopping ${first.id} carried other arguments`).toEqual({ job_ids: [first.id], cancel: true })
  }

  await pressStop(pane, world, second.id)
  expect(answered(), `stopping ${second.id} did not add one monitor call for it alone`).toEqual([
    { job_ids: [first.id], cancel: true },
    { job_ids: [second.id], cancel: true },
  ])
  await pane.unmount()
})

// --- 5. finish and cancel notices ---

test('M3a-5a: a job leaving running raises one notice naming it, once, whether it turned done, was collected or was cancelled', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const finishes = ownJob(t, 'd-mvzf5aaa-0', 'D1', 300_000)
  const collected = ownJob(t, 'd-mvzf5bbb-1', 'cld2', 200_000)
  const cancelled = ownJob(t, 'd-mvzf5ccc-2', 'glm1', 100_000)
  const foreign = { ...ownJob(t, 'd-mvze5xxx-0', 'D3', 250_000), hostSession: OTHER }
  world.job(foreign)
  for (const job of [finishes, collected, cancelled]) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const own = [finishes, collected, cancelled]
  const before = new Map(own.map(j => [j.id, noticesNaming(world, j.id)]))

  const at = world.clock.now()
  world.job({ ...finishes, state: 'done', doneAt: at })
  world.deliver(collected.id, collected.profile)
  world.job({ ...cancelled, state: 'done', doneAt: at, cancelled: true })
  world.job({ ...foreign, state: 'done', doneAt: at })
  await world.clock.advance(POLL_MS)
  for (const job of own) {
    const raised = noticesNaming(world, job.id) - before.get(job.id)!
    expect(raised, `${job.id} left running and ${raised} notices named it, not one`).toBe(1)
  }

  // Collecting the finished and the cancelled job later raises nothing more.
  world.deliver(finishes.id, finishes.profile)
  world.deliver(cancelled.id, cancelled.profile)
  await world.clock.advance(3 * POLL_MS)
  for (const job of own) {
    const raised = noticesNaming(world, job.id) - before.get(job.id)!
    expect(raised, `${job.id} was noticed ${raised} times, not exactly once`).toBe(1)
  }
  expect(noticesNaming(world, foreign.id), `a notice named ${foreign.id}, another session's job`).toBe(0)
})

test('M3a-5b: a record that reads cancelled or stopped counts as leaving running, noticed once each', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const cancelled = ownJob(t, 'd-mvzf5ddd-0', 'D1', 300_000)
  const stopped = ownJob(t, 'd-mvzf5eee-1', 'D2', 200_000)
  for (const job of [cancelled, stopped]) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const before = new Map([cancelled, stopped].map(j => [j.id, noticesNaming(world, j.id)]))

  world.job({ ...cancelled, state: 'cancelled', doneAt: world.clock.now() })
  world.job({ ...stopped, state: 'stopped', doneAt: world.clock.now() })
  await world.clock.advance(3 * POLL_MS)
  for (const job of [cancelled, stopped]) {
    const raised = noticesNaming(world, job.id) - before.get(job.id)!
    expect(raised, `${job.id} reads ${job === cancelled ? 'cancelled' : 'stopped'} and ${raised} notices named it, not one`).toBe(1)
  }
})

// --- 6. finished rows kept ---

test('M3a-6: a collected job stays listed as finished for the rest of the session', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  // One collected straight from running, one that turned done first.
  const direct = ownJob(t, 'd-mvzf6aaa-0', 'D1', 300_000)
  const viaDone = ownJob(t, 'd-mvzf6bbb-1', 'cld2', 400_000)
  for (const job of [direct, viaDone]) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  for (const job of [direct, viaDone]) await expectEntry(pane, world, job)

  const doneAt = world.clock.now()
  world.job({ ...viaDone, state: 'done', doneAt })
  pane = await advanceUndrawn($, world, pane)
  world.deliver(direct.id, direct.profile)
  world.deliver(viaDone.id, viaDone.profile)
  pane = await advanceUndrawn($, world, pane)
  const expectFinished = async (when: string) => {
    for (const job of [direct, viaDone]) {
      const entry = entryFor(await drawn(pane), job.id)
      expect(entry !== undefined, `${when}, the pane no longer lists ${job.id}`).toBe(true)
      const text = textOf(entry)
      expect(hasWord(text, job.profile), `${when}, the entry for ${job.id} lost its account ${job.profile}: ${clip(text)}`).toBe(true)
      const state = await headerText(pane, world, job.id)
      expect(FINISHED_WORD.test(state), `${when}, the header for ${job.id} does not say done: ${clip(state)}`).toBe(true)
      expect(RUNNING_WORD.test(state), `${when}, the header for ${job.id} still says running: ${clip(state)}`).toBe(false)
    }
  }
  await expectFinished('once collected')

  // clauth's sweep reaps the ledgers later; the session goes on.
  world.remove(`${JOBS}/${direct.id}.json.delivered`)
  world.remove(`${JOBS}/${viaDone.id}.json.delivered`)
  await world.clock.advance(10 * POLL_MS)
  await pane.unmount()
  pane = await mountPane($, world)
  await expectFinished('after the ledgers were reaped and the pane drawn again')
  await pane.unmount()
})

// --- 7. on by default ---

test('M3a-7: with no option set, one delegate call opens no pane by itself, and the job is listed and its finish noticed', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzf7aaa-0', 'D1', 30_000)
  await delegate($, world, job)
  await world.clock.settle()
  expect(world.opens.length, 'with no option set, a delegate call opened the pane by itself').toBe(0)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  await expectEntry(pane, world, job)
  const before = noticesNaming(world, job.id)
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane)
  expect(noticesNaming(world, job.id) - before, `with no option set, ${job.id} finished and no single notice named it`).toBe(1)
  await pane.unmount()
})

// --- 8. reopening the pane: a `/delegates` command ---

// The engine beneath a slash command the mod passes on: it prints nothing.
function installCommands(on: On): void {
  on('command.run', () => ({ text: '' }))
}

test('M3a-8a: `/delegates` reopens the delegates pane after it was closed', { plugins: [PANE_CLOSER] }, async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzf8aaa-0', 'D1', 1_000))
  await world.clock.settle()
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const first = world.opens.at(-1)
  expect(first !== undefined, 'the first `/delegates` opened no pane').toBe(true)
  const id = first!.id

  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(id), 'the pane stayed open after it was closed').toBe(false)
  const opensAfterClose = world.opens.length

  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  expect(world.opens.length - opensAfterClose, '`/delegates` did not open a pane').toBe(1)
  expect(world.opens.at(-1)!.id, '`/delegates` opened another pane than the delegates pane').toBe(id)
  expect(world.open.has(id), 'the delegates pane is not open after `/delegates`').toBe(true)
})

test('M3a-8b: `/delegates` before any delegate call opens the pane, listing no job', async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  expect(world.opens.length, '`/delegates` with no delegate yet did not open the pane').toBe(1)
  const pane = await mountPane($, world)
  expect(buttonsIn(await drawn(pane)).length, 'the empty pane draws a row control').toBe(0)
  await pane.unmount()
})

// --- 9. reopening the pane: the band's control above the prompt, the hint line under it the engine's ---

// The engine's own hint line, as the test's hook draws it beneath the mod: a
// mod that keeps the engine's line keeps this text.
const ENGINE_HINT = 'esc to interrupt · 2 shells'

type Hint = Mounted<'terminal', 'PromptHint'>

// Registered before the world, so it answers the hint line ahead of the
// world's catch-all drawing; the no-delegate case shows it is the one reached.
function installEngineHint(on: On): void {
  on('ui.render', { component: 'PromptHint' }, () => ({ type: 'Text', props: {}, children: [ENGINE_HINT] }))
}

async function mountHint($: Engine): Promise<Hint> {
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'PromptHint',
    props: { isDraft: false, isWorking: true, hint: ENGINE_HINT },
  })
}

const ABOVE_PROMPT_PROPS = {
  hasSurvey: false,
  isCollapsed: false,
  isWorking: true,
  maxRows: 12,
  bodyColumns: 160,
  scroll: { offset: 0, bodyRows: 12 },
  view: {},
}

type AbovePrompt = Mounted<'terminal', 'AbovePrompt'>

// The band above the prompt, on a fullscreen terminal that reports clicks.
async function mountAbovePrompt($: Engine): Promise<AbovePrompt> {
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'AbovePrompt',
    props: ABOVE_PROMPT_PROPS,
    viewport: { columns: ABOVE_PROMPT_PROPS.bodyColumns + 5, rows: 50, isFullscreen: true },
  })
}

// The band's count controls: each Button carrying `N delegate(s)`.
const countControlsIn = (root: unknown): Drawn[] => buttonsIn(root).filter(b => /\b\d+ delegates?\b/.test(textOf(b)))

// A delegate's chip in the band: the Button naming its account.
function bandChipFor(root: unknown, account: string): Drawn | undefined {
  return buttonsIn(root).find(b => hasWord(textOf(b), account))
}

test('M3a-9a: with no delegate in the session the hint line is the engine\'s own, with no control added', async ($, on) => {
  installEngineHint(on)
  installWorld(on)
  await startSession($)
  const hint = await mountHint($)
  await hint.redraw()
  const tree = await hint.drawn()
  expect(textOf(tree).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(JSON.stringify(tree) ?? '')}`).toBe(true)
  expect(buttonsIn(tree).length, 'a control shows in the hint line with no delegate in the session').toBe(0)
  await hint.unmount()
})

test('M3a-9b: once the session has a delegate the hint line keeps the engine\'s line and the band above the prompt holds a control that opens the delegates pane', { plugins: [PANE_CLOSER] }, async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzf9aaa-0', 'D1', 1_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const id = 'clauth-delegates'
  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(id), 'the pane stayed open after it was closed').toBe(false)

  const hint = await mountHint($)
  await hint.redraw()
  const line = await hint.drawn()
  expect(textOf(line).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(textOf(line))}`).toBe(true)
  await hint.unmount()

  const band = await mountAbovePrompt($)
  await band.redraw()
  const tree = await band.drawn()
  const controls = countControlsIn(tree)
  expect(controls.length, `the band holds no single count control for the delegates pane: ${clip(textOf(tree))}`).toBe(1)
  const opensBefore = world.opens.length
  await band.press({ key: keyOf(controls[0]!) })
  await world.clock.settle()
  expect(world.opens.length - opensBefore, 'pressing the band\'s count control opened no pane').toBe(1)
  expect(world.opens.at(-1)!.id, 'the band\'s count control opened another pane than the delegates pane').toBe(id)
  await band.unmount()
})

// --- 10. the store's edge shapes: claims, dead owners, cancels, `/clear`, slugs ---

// The store's read cadence: the grace and the owner probe count scans, so a
// test that wants exactly one has to move the clock by one tick.
const TICK_MS = 2_000

// A transcript the last 10 lines cannot reach the middle of: a 600 KB filler
// block sits before the marker line, so only a byte-bounded read finds it.
function longTranscriptFile(sessionId: string, cwd: string): string {
  const base = { sessionId, cwd, version: '2.1.322', isSidechain: false }
  const lines: string[] = []
  let n = 0
  const row = (text: string) => {
    n += 1
    lines.push(
      JSON.stringify({
        type: 'assistant',
        message: { role: 'assistant', content: [{ type: 'text', text }] },
        ...base,
        uuid: `long-u${n}`,
        timestamp: new Date(T0 - 900_000 + n * 1000).toISOString(),
      }),
    )
  }
  row('long-user-first')
  for (let i = 0; i < 30; i += 1) row(`${'z'.repeat(20_000)}-${i}`)
  row('long-tail-marker')
  for (let i = 0; i < 12; i += 1) row(`long-filler-${i}`)
  row('long-reply-final')
  return `${lines.join('\n')}\n`
}

test('M3a-A: a collected job whose delivery ledger lands after one scan reads done, not ended', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  // Another session's record, so the job store stays a directory the mock
  // world can list once this job's record is gone.
  world.job({ ...ownJob(world.clock.now(), 'd-mvzfa900-9', 'D3', 120_000), hostSession: OTHER })
  const job = ownJob(world.clock.now(), 'd-mvzfa001-0', 'D1', 300_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  await expectEntry(pane, world, job)

  // clauth's claim renames the record away before the ledger lands: the scan
  // in that window must not settle the delivery as ended.
  world.remove(`${JOBS}/${job.id}.json`)
  pane = await advanceUndrawn($, world, pane, { ms: 2 * TICK_MS })
  const midway = await headerText(pane, world, job.id)
  expect(RUNNING_WORD.test(midway), `the header for ${job.id} settled while its delivery ledger was still being written: ${clip(midway)}`).toBe(true)
  expect(noticesNaming(world, job.id), `a notice named ${job.id} before its delivery ledger landed`).toBe(0)

  world.put(`${JOBS}/${job.id}.json.delivered`, ledgerFile(job.id, job.profile, world.clock.now()))
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(FINISHED_WORD.test(text), `the collected ${job.id} does not say done: ${clip(text)}`).toBe(true)
  expect(/\bended\b/.test(text), `the collected ${job.id} reads ended, not done: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\bended\b/.test(notices[0]!.text), `the notice for ${job.id} reads ended: ${clip(notices[0]!.text)}`).toBe(false)
  await pane.unmount()
})

test('M3a-B: a live run whose owner process is gone reads orphaned once and stops the ticks', async ($, on) => {
  // Every record stamps owner 5151; this world answers that pid as gone and
  // lets the world's own handler answer every other command.
  on('process.run', { argv: [/^ps$/] }, ($, e, next) =>
    e.argv.includes('5151') ? processResult(1, '') : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfb001-0', 'D1', 300_000)
  await delegate($, world, job)
  await world.clock.advance(2 * POLL_MS)
  const pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the running ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\borphaned\b/.test(text), `the run whose owner is gone does not read orphaned: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the dead run still reads running: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\borphaned\b/.test(notices[0]!.text), `the notice for ${job.id} does not say orphaned: ${clip(notices[0]!.text)}`).toBe(true)

  // With the dead run settled, the ticks stop: a record written afterwards is
  // never read.
  const late = ownJob(world.clock.now(), 'd-mvzfb002-1', 'D2', 1_000)
  world.job(late)
  await world.clock.advance(2 * POLL_MS)
  expectUnlisted(await drawn(pane), late.id, `the pane lists ${late.id} although nothing live should scan`)
  expect(noticesNaming(world, late.id), `a notice named ${late.id} although nothing live should scan`).toBe(0)
  await pane.unmount()
})

test('a delegate its session exit stopped reads stopped, not done', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfs001-0', 'D1', 300_000, {
    state: 'done',
    doneAt: world.clock.now() - 60_000,
    stopped: true,
  })
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\bstopped\b/.test(text), `the stop at the session's exit reads done, not stopped: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the stopped ${job.id} still reads running: ${clip(text)}`).toBe(false)
  expect(await offersStop(pane, world, job.id), `the stopped ${job.id} still offers a stop control: ${clip(text)}`).toBe(false)
  await pane.unmount()
})

test('the sweep\'s stopped tombstone reads stopped, not orphaned', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfs002-0', 'D1', 300_000, {
    state: 'done',
    doneAt: world.clock.now() - 60_000,
    stopped: true,
    crashed: true,
  })
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the swept ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\bstopped\b/.test(text), `the sweep's tombstone reads orphaned, not stopped: ${clip(text)}`).toBe(true)
  expect(/\borphaned\b/.test(text), `the stopped ${job.id} still reads orphaned: ${clip(text)}`).toBe(false)
  await pane.unmount()
})

test('a row the pane marked orphaned moves to stopped when its record later reads it', async ($, on) => {
  on('process.run', { argv: [/^ps$/] }, ($, e, next) =>
    e.argv.includes('5151') ? processResult(1, '') : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  const now = world.clock.now()
  const job = ownJob(now, 'd-mvzfs003-0', 'D1', 300_000)
  await delegate($, world, job)
  // A second live run keeps the pane's ticks going once the orphan settles.
  const keeper = ownJob(now, 'd-mvzfs004-1', 'D2', 60_000)
  world.put(`${JOBS}/${keeper.id}.json`, jobFileWith(keeper, { owner_pid: 4242 }))
  await world.clock.advance(2 * POLL_MS)
  let pane = await mountPane($, world)
  const orphaned = await headerText(pane, world, job.id)
  expect(/\borphaned\b/.test(orphaned), `the run whose owner is gone does not read orphaned: ${clip(orphaned)}`).toBe(true)
  expect(noticesNaming(world, job.id), `the orphaned ${job.id} raised no notice`).toBe(1)

  // The startup sweep converts the corpse into its stopped tombstone.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() - 1_000, stopped: true, crashed: true })
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost ${job.id} when its record became the stopped tombstone`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\bstopped\b/.test(text), `the converted ${job.id} still reads orphaned, not stopped: ${clip(text)}`).toBe(true)
  expect(/\borphaned\b/.test(text), `the converted ${job.id} still reads orphaned: ${clip(text)}`).toBe(false)
  expect(noticesNaming(world, job.id), `the row moving to stopped raised a second notice naming ${job.id}`).toBe(1)
  await pane.unmount()
})

test('a live row whose stop is collected into its ledger settles stopped, not done', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfs005-0', 'D1', 300_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the running ${job.id}`).toBe(true)

  // clauth collects the stopped record between two reads: the record goes,
  // its ledger stays with the stop marker the claim judged.
  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, stoppedLedgerFile(job.id, job.profile, world.clock.now()))
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })

  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\bstopped\b/.test(text), `the stop collected into its ledger reads done, not stopped: ${clip(text)}`).toBe(true)
  expect(/\bdone\b/.test(text), `the stopped ${job.id} still reads done: ${clip(text)}`).toBe(false)
  expect(RUNNING_WORD.test(text), `the stopped ${job.id} still reads running: ${clip(text)}`).toBe(false)
  expect(await offersStop(pane, world, job.id), `the stopped ${job.id} still offers a stop control: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\bstopped\b/.test(notices[0]!.text), `the notice for ${job.id} does not say stopped: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

test('a ledger an older build wrote, with no stopped field, settles its collected row as done', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfs006-1', 'D2', 300_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)

  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, ledgerFile(job.id, job.profile, world.clock.now()))
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })

  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(FINISHED_WORD.test(text), `the collected ${job.id} does not say done: ${clip(text)}`).toBe(true)
  expect(/\bstopped\b/.test(text), `a ledger an older build wrote reads stopped: ${clip(text)}`).toBe(false)
  expect(/\bended\b/.test(text), `the collected ${job.id} reads ended, not done: ${clip(text)}`).toBe(false)
  await pane.unmount()
})

test('an orphaned row whose stop is collected into its ledger reads stopped', async ($, on) => {
  on('process.run', { argv: [/^ps$/] }, ($, e, next) =>
    e.argv.includes('5151') ? processResult(1, '') : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  const now = world.clock.now()
  const job = ownJob(now, 'd-mvzfs007-0', 'D1', 300_000)
  await delegate($, world, job)
  const keeper = ownJob(now, 'd-mvzfs008-1', 'D2', 60_000)
  world.put(`${JOBS}/${keeper.id}.json`, jobFileWith(keeper, { owner_pid: 4242 }))
  await world.clock.advance(2 * POLL_MS)
  let pane = await mountPane($, world)
  const orphaned = await headerText(pane, world, job.id)
  expect(/\borphaned\b/.test(orphaned), `fixture control: not orphaned: ${clip(orphaned)}`).toBe(true)

  // One monitor call by id: the reader sweep converts the corpse and the claim
  // collects it, so the pane never reads the tombstone, only the ledger.
  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, stoppedLedgerFile(job.id, job.profile, world.clock.now()))
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  const text = await headerText(pane, world, job.id)
  expect(/\bstopped\b/.test(text), `the collected stop still reads: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3a-C: a blocking run stopped through its call reads cancelled, not done', async ($, on) => {
  const blockedSession = 'sess-blocked'
  // The blocking call's own settle: clauth deletes the liveness record and
  // returns a cancelled envelope, whose prose is all a tool result carries.
  let armed = false
  on('tool.call', { tool: DELEGATE }, ($, e, next) =>
    armed
      ? ({
          isError: true,
          result: 'cancelled',
          text: `delegate to \`D1\` cancelled after 42s: delegate cancelled after 42s; resume with session id \`${blockedSession}\``,
        } as ToolCallResult)
      : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  // A live background run keeps the store's ticks going while the blocking
  // run is in flight.
  const keeper = ownJob(world.clock.now(), 'd-mvzfc000-0', 'D2', 30_000)
  await delegate($, world, keeper)
  const blocking = ownJob(world.clock.now(), 'd-mvzfc001-1', 'D1', 120_000, { sessionId: blockedSession })
  world.put(`${JOBS}/${blocking.id}.live.json`, jobFile(blocking))
  world.blockingIds.add(blocking.id)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  const shownBlocking = await headerText(pane, world, blocking.id)
  expect(/\bblocking\b/.test(shownBlocking), `the pane draws no blocking header for ${blocking.id}: ${clip(shownBlocking)}`).toBe(true)

  // The stop lands while the call is in flight; the call's own settle then
  // carries the cancel.
  armed = true
  world.remove(`${JOBS}/${blocking.id}.live.json`)
  const settled = $.tool.call({ tool: DELEGATE, profile: blocking.profile, prompt: 'block', background: false })
  await pressStop(pane, world, blocking.id)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(blocking.id)).length,
    `stopping the blocking ${blocking.id} did not ask clauth's monitor`,
  ).toBe(1)
  await settled
  await world.clock.settle()
  pane = await advanceUndrawn($, world, pane)

  expect(entryFor(await drawn(pane), blocking.id) !== undefined, `the pane lost the blocking ${blocking.id}`).toBe(true)
  const text = await headerText(pane, world, blocking.id)
  expect(/\bcancelled\b/.test(text), `the stopped blocking run does not read cancelled: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the stopped blocking run still reads running: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, blocking.id))
  expect(notices.length, `${blocking.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\bcancelled\b/.test(notices[0]!.text), `the notice for ${blocking.id} does not say cancelled: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

test('M3a-D: a delegate that outlives a /clear keeps its row, stop and finish notice', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const world = installWorld(on)
  await startSession($)
  const before = ownJob(world.clock.now(), 'd-mvzfd001-0', 'D1', 300_000)
  world.job(before)

  // The engine's /clear shape: the session ends under the old id, no
  // `session.start` follows, and the process goes on under a new one.
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })

  const fresh = ownJob(world.clock.now(), 'd-mvzfd002-1', 'D2', 60_000, { hostSession: 'sid-after-clear' })
  await delegate($, world, fresh)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  const tree = await drawn(pane)
  expect(entryFor(tree, before.id) !== undefined, `the /clear lost the running ${before.id}: ${clip(textOf(tree))}`).toBe(true)
  expect(entryFor(tree, fresh.id) !== undefined, `the /clear's own ${fresh.id} is not listed`).toBe(true)

  await pressStop(pane, world, before.id)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(before.id)).length,
    `the pre-clear run ${before.id} has no working stop after the /clear`,
  ).toBe(1)

  world.job({ ...before, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane)
  const notices = world.notices.filter(n => hasWord(n.text, before.id))
  expect(notices.length, `${before.id} was noticed ${notices.length} times across the /clear, not once`).toBe(1)
  expect(FINISHED_WORD.test(notices[0]!.text), `the notice for ${before.id} does not say done: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

test('M3a-E1: /delegates reads the store, so a job written without a delegate call is listed', async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfe001-0', 'D1', 60_000)
  world.job(job)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const pane = await mountPane($, world)
  await expectEntry(pane, world, job)
  await pane.unmount()
})

test('M3a-E2: the band reads the store, so a job written without a delegate call gets its chip', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  world.job(ownJob(world.clock.now(), 'd-mvzfe002-0', 'D1', 60_000))
  const band = await mountAbovePrompt($)
  await world.clock.settle()
  await band.redraw()
  const tree = await band.drawn()
  expect(countControlsIn(tree).length, `the band shows no single count control for a job written without a delegate call: ${clip(textOf(tree))}`).toBe(1)
  expect(bandChipFor(tree, 'D1') !== undefined, `the band shows no chip for a job written without a delegate call: ${clip(textOf(tree))}`).toBe(true)
  await band.unmount()

  const hint = await mountHint($)
  await hint.redraw()
  const line = await hint.drawn()
  expect(textOf(line).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(textOf(line))}`).toBe(true)
  await hint.unmount()
})

test('M3a-F: a working directory whose project slug passes 200 characters finds its transcript', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const cwd = `/work/${'a'.repeat(240)}`
  // The engine's own name for that path: the slug's first 200 characters, a
  // dash, then the path's 32-bit hash in base 36.
  const dir = `-work-${'a'.repeat(194)}-5g918x`
  const job = ownJob(world.clock.now(), 'd-mvzff001-0', 'D1', 300_000, { sessionId: 'sess-long', cwd })
  world.put(`${PROJECTS}/${dir}/sess-long.jsonl`, transcriptFile('long', 'sess-long', cwd))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  pane = await openEntry($, world, pane, job.id)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'long-user-final'), `the view of ${job.id} misses its transcript's last user line: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'long-reply-final'), `the view of ${job.id} misses its transcript's last reply: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3a-G: the stop asks the spelling the delegate tool\'s own name implies first', async ($, on) => {
  const asked: string[] = []
  on('mcp.call', { server: 'plugin_clauth_clauth' }, ($, e, next) =>
    e.tool === 'monitor'
      ? (asked.push(e.server), { value: { content: [{ type: 'text' as const, text: 'cancel requested' }], isError: false } })
      : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfg001-0', 'D1', 120_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  await pressStop(pane, world, job.id)
  expect(asked.length, 'the stop did not ask the tool-name spelling of the delegate tool').toBe(1)
  expect(world.mcpCalls.filter(c => c.tool === 'monitor').length, 'the stop reached an /mcp spelling before the tool-name one').toBe(0)
  expect(noticesNaming(world, job.id), 'the stop raised a notice').toBe(0)
  await pane.unmount()
})

test('M3a-H1: the store\'s ticks stop once nothing is live, and a delegate call restarts them', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfh001-0', 'D1', 60_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane)
  expect(noticesNaming(world, job.id), `the finished ${job.id} was not noticed`).toBe(1)

  const late = ownJob(world.clock.now(), 'd-mvzfh002-1', 'D2', 1_000)
  world.job(late)
  await world.clock.advance(3 * POLL_MS)
  expectUnlisted(await drawn(pane), late.id, `the pane lists ${late.id}, so the ticks never stopped`)
  expect(noticesNaming(world, late.id), `a notice named ${late.id}, so the ticks never stopped`).toBe(0)

  // The control: a new delegate call starts the ticks again, and the same
  // record is then read. The call's own scan lists the record without a
  // tick, so only the finish that lands after it proves a tick ran.
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfh003-2', 'D3', 1_000))
  world.job({ ...late, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane)
  const restarted = await drawn(pane)
  expect(entryFor(restarted, late.id) !== undefined, `a delegate call no longer restarts the store's ticks, so ${late.id} stays unread: ${clip(textOf(restarted))}`).toBe(true)
  expect(noticesNaming(world, late.id), `a delegate call no longer restarts the store's ticks, so ${late.id} finished unnoticed`).toBe(1)
  await pane.unmount()
})

test('M3a-H2: the transcript is read from its last 512 KiB by bytes, not by lines', async ($, on) => {
  const sources = new Map<string, string>()
  // A `tail` that answers `-c` by bytes: the world's own reads only lines, so
  // a byte window looks exactly like a line window to it.
  on('process.run', { argv: [/^tail$/] }, ($, e) => {
    const argv = e.argv
    const path = argv[argv.length - 1] ?? ''
    const text = sources.get(norm(path))
    if (text === undefined) return processResult(1, '', `tail: ${path}: No such file or directory`)
    let bytes: number | null = null
    let lines: number | null = null
    for (let i = 1; i < argv.length - 1; i += 1) {
      const arg = argv[i]!
      if (arg === '-c' && argv[i + 1] !== undefined) bytes = Number(argv[i + 1])
      else if (/^-c\d+$/.test(arg)) bytes = Number(arg.slice(2))
      else if (arg === '-n' && argv[i + 1] !== undefined) lines = Number(argv[i + 1])
      else if (/^-n\d+$/.test(arg)) lines = Number(arg.slice(2))
    }
    if (bytes !== null) {
      const all = new TextEncoder().encode(text)
      return processResult(0, new TextDecoder().decode(all.slice(Math.max(0, all.length - bytes))))
    }
    return processResult(0, tailLines(text, lines ?? 10))
  })
  const world = installWorld(on)
  await startSession($)
  const cwd = '/work/long'
  const job = ownJob(world.clock.now(), 'd-mvzfh004-0', 'D1', 300_000, { sessionId: 'sess-bytes', cwd })
  const file = transcriptPath(cwd, 'sess-bytes')
  const text = longTranscriptFile('sess-bytes', cwd)
  sources.set(norm(file), text)
  world.put(file, text)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  // A card per message outgrows a 40-row pane before the marker; the subject
  // here is the byte-bounded read, so the pane gets rows for all it read.
  await askForPane($, world)
  let pane = await mountTallPane($, world)
  await selectEntry(pane, world, job.id)
  pane = await advanceUndrawn($, world, pane, { mount: mountTallPane })
  const shown = textOf(await drawn(pane))
  expect(hasWord(shown, 'long-tail-marker'), `the view misses the line inside the last 512 KiB: ${clip(shown)}`).toBe(true)
  expect(hasWord(shown, 'long-reply-final'), `the view misses the transcript's last reply: ${clip(shown)}`).toBe(true)
  await pane.unmount()
})

test('M3a-I: closing the pane drops the chosen delegate\'s view, so a reopen does not land on it and no read runs', { plugins: [PANE_CLOSER] }, async ($, on) => {
  const tails: string[][] = []
  // Counts the transcript reads the mod runs, answering them from the world.
  on('process.run', { argv: [/^tail$/] }, ($, e, next) => {
    tails.push([...e.argv])
    return next(e)
  })
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const cwd = '/work/probe'
  const job = ownJob(world.clock.now(), 'd-mvzfi001-0', 'D1', 300_000, { sessionId: 'sess-close', cwd })
  const file = transcriptPath(cwd, 'sess-close')
  world.put(file, transcriptFile('closing', 'sess-close', cwd))
  await delegate($, world, job)
  // A newer delegate is the one an open lands on, so a reopen that kept the
  // chosen view is told apart from one that dropped it.
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfi002-1', 'D2', 60_000))
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  pane = await openEntry($, world, pane, job.id)
  const shown = textOf(await drawn(pane))
  expect(hasWord(shown, 'closing-reply-final'), `the view of ${job.id} shows no transcript: ${clip(shown)}`).toBe(true)
  expect(tails.length > 0, 'the view read no transcript').toBe(true)

  world.put(
    file,
    `${transcriptFile('closing', 'sess-close', cwd)}${JSON.stringify({
      type: 'assistant',
      message: { role: 'assistant', content: [{ type: 'text', text: 'closing-after-close' }] },
      sessionId: 'sess-close',
      cwd,
    })}\n`,
  )
  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(world.opens.at(-1)!.id), 'the pane stayed open after it was closed').toBe(false)
  const readsAfterClose = tails.length
  pane = await advanceUndrawn($, world, pane)
  expect(tails.length, `${tails.length - readsAfterClose} transcript reads ran with the pane closed`).toBe(readsAfterClose)
  await pane.unmount()

  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const reopened = await mountPane($, world)
  const tree = await drawn(reopened)
  expect(entryFor(tree, job.id) !== undefined, `the reopened pane does not list ${job.id}`).toBe(true)
  expect(hasWord(textOf(tree), 'closing-reply-final'), 'the reopened pane still shows the transcript view').toBe(false)
  await reopened.unmount()
})

// --- 11. `/clear` carry-over, scan cadence, probe failures, cancel attribution ---

// The engine's `/clear` drops the values the host held for the conversation.
// The kit keeps them, so a test that models a clear answers the delegates
// read empty until the mod writes its rows again.
function installStateDrop(on: On): { drop: () => void } {
  let dropped = false
  on('state.get', async ($, e, next) => {
    const read = (await next(e)) as { value: { value: unknown; version: number } }
    return dropped && e.plugin === 'clauth' && e.key === 'delegates'
      ? { value: { value: undefined, version: read.value.version } }
      : read
  })
  on('state.set', ($, e, next) => {
    if (e.plugin === 'clauth' && e.key === 'delegates') dropped = false
    return next(e)
  })
  return {
    drop: () => {
      dropped = true
    },
  }
}

// A record as clauth wrote it, with fields the job helper always stamps
// replaced: an ownerless record an older server left has no owner pid.
function jobFileWith(j: Job, patch: Record<string, unknown>): string {
  return JSON.stringify({ ...(JSON.parse(jobFile(j)) as Record<string, unknown>), ...patch })
}

test('M3a-J: a /clear re-adopts a pre-clear run whose record was read before it and never moved', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const stateDrop = installStateDrop(on)
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfj001-0', 'D1', 300_000)
  await delegate($, world, job)
  // One tick reads the record and stores its stamp before the clear.
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  await expectEntry(pane, world, job)

  // The engine's /clear: a new session id, no `session.start`, and the rows
  // the ended conversation's session held are gone.
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()

  // The record has not moved; the ticks keep reading for it.
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  const tree = await drawn(pane)
  expect(
    entryFor(tree, job.id) !== undefined,
    `the /clear lost the running ${job.id} whose record was read before it: ${clip(textOf(tree))}`,
  ).toBe(true)
  expect(
    RUNNING_WORD.test(await headerText(pane, world, job.id)),
    `the re-adopted ${job.id} does not read running`,
  ).toBe(true)

  await pressStop(pane, world, job.id)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(job.id)).length,
    `the re-adopted ${job.id} has no working stop`,
  ).toBe(1)

  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times across the /clear, not once`).toBe(1)
  expect(FINISHED_WORD.test(notices[0]!.text), `the notice for ${job.id} does not say done: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

test('M3a-K: the ids a /clear ended do not carry into the next session in this process', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })

  // The process outlives the session: a new one starts under a new id.
  sid = 'sid-next'
  await $.session.start({ cwd: HOST_CWD, surface: 'terminal', isInteractive: true })

  // A run left live under the cleared conversation belongs to no session here.
  const old = ownJob(world.clock.now(), 'd-mvzfk001-0', 'D1', 120_000)
  world.job(old)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const pane = await mountPane($, world)
  expectUnlisted(await drawn(pane), old.id, `the next session adopted ${old.id} from the conversation the /clear ended`)
  await pane.unmount()
})

test('M3a-L1: the band asks the store once a second while the ticks are stopped, not on every draw', async ($, on) => {
  const lists: string[] = []
  on('fs.list', { path: JOBS }, ($, e, next) => {
    lists.push(e.path)
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const band = await mountAbovePrompt($)
  await world.clock.settle()
  const afterFirst = lists.length

  for (let i = 0; i < 3; i += 1) {
    await world.clock.advance(300)
    await band.redraw()
    await world.clock.settle()
  }
  expect(
    lists.length - afterFirst,
    `the band listed the store on each of three draws inside a second: ${lists.length - afterFirst} listings`,
  ).toBe(0)

  // A draw a second later still finds a job minted outside a delegate call.
  await world.clock.advance(1000)
  world.job(ownJob(world.clock.now(), 'd-mvzfl001-0', 'D1', 60_000))
  await band.redraw()
  await world.clock.settle()
  await band.redraw()
  const tree = await band.drawn()
  expect(countControlsIn(tree).length, `the throttled band never read the store: ${clip(textOf(tree))}`).toBe(1)
  expect(bandChipFor(tree, 'D1') !== undefined, `the throttled band draws no chip for the minted D1 job: ${clip(textOf(tree))}`).toBe(true)
  await band.unmount()
})

test('M3a-L2: while the ticks run the band adds no store read of its own', async ($, on) => {
  const lists: string[] = []
  on('fs.list', { path: JOBS }, ($, e, next) => {
    lists.push(e.path)
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfl002-0', 'D1', 60_000))
  await world.clock.advance(POLL_MS)
  const band = await mountAbovePrompt($)
  await world.clock.settle()
  await band.redraw()
  await world.clock.settle()
  // A band that draws nothing for the live run would add no read either.
  const tree = await band.drawn()
  expect(countControlsIn(tree).length, `the band draws no single count control for the live run: ${clip(textOf(tree))}`).toBe(1)
  const before = lists.length
  for (let i = 0; i < 3; i += 1) {
    await world.clock.advance(1000)
    await band.redraw()
    await world.clock.settle()
  }
  // Three seconds hold one tick's own read; a draw adds none.
  expect(
    lists.length - before,
    `the band read the store ${lists.length - before} times over three draws while the ticks ran`,
  ).toBe(1)
  await band.unmount()
})

test('M3a-M: a ps that rejects the owner probe\'s form never reads a live run as orphaned', async ($, on) => {
  on('process.run', { argv: [/^ps$/] }, () => processResult(1, '', 'ps: unknown user-defined format specifier "stat"'))
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfm001-0', 'D1', 300_000)
  await delegate($, world, job)
  await world.clock.advance(2 * POLL_MS)
  const pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the running ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(
    /\borphaned\b/.test(text),
    `a ps that rejects the probe's form read the live ${job.id} as orphaned: ${clip(text)}`,
  ).toBe(false)
  expect(RUNNING_WORD.test(text), `the live ${job.id} no longer reads running: ${clip(text)}`).toBe(true)
  expect(noticesNaming(world, job.id), `a notice named the live ${job.id}`).toBe(0)
  await pane.unmount()
})

test('M3a-N: a pre-spawn cancel names no run, so an unrelated blocking row settles done', async ($, on) => {
  const unrelated = 'sess-unrelated'
  let armed = false
  on('tool.call', { tool: DELEGATE }, ($, e, next) =>
    armed
      ? ({
          isError: true,
          result: 'cancelled',
          text: "delegate to `D1` cancelled after 3s: delegate cancelled after 3s, before it spawned: the account's window was not spent",
        } as ToolCallResult)
      : next(e),
  )
  const world = installWorld(on)
  await startSession($)
  const keeper = ownJob(world.clock.now(), 'd-mvzfn000-0', 'D2', 30_000)
  await delegate($, world, keeper)
  const blocking = ownJob(world.clock.now(), 'd-mvzfn001-1', 'D1', 120_000, { sessionId: unrelated })
  world.put(`${JOBS}/${blocking.id}.live.json`, jobFile(blocking))
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  expect(
    /\bblocking\b/.test(await headerText(pane, world, blocking.id)),
    `the pane draws no blocking header for ${blocking.id}`,
  ).toBe(true)

  // The call settles cancelled before its own run spawned: the prose names no
  // session, so no row in the pane is its.
  armed = true
  world.remove(`${JOBS}/${blocking.id}.live.json`)
  await $.tool.call({ tool: DELEGATE, profile: 'D1', prompt: 'block', background: false })
  await world.clock.settle()
  pane = await advanceUndrawn($, world, pane, { ms: 3 * TICK_MS })

  expect(entryFor(await drawn(pane), blocking.id) !== undefined, `the pane lost the blocking ${blocking.id}`).toBe(true)
  const text = await headerText(pane, world, blocking.id)
  expect(/\bcancelled\b/.test(text), `a pre-spawn cancel marked the unrelated ${blocking.id} cancelled: ${clip(text)}`).toBe(false)
  expect(FINISHED_WORD.test(text), `the unrelated ${blocking.id} does not settle done: ${clip(text)}`).toBe(true)
  const notices = world.notices.filter(n => hasWord(n.text, blocking.id))
  expect(notices.length, `${blocking.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\bcancelled\b/.test(notices[0]!.text), `the notice for ${blocking.id} says cancelled: ${clip(notices[0]!.text)}`).toBe(false)
  await pane.unmount()
})

test('M3a-O: a store read that adopts a live run restarts the ticks', async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfo001-0', 'D1', 60_000)
  world.job(job)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  let pane = await mountPane($, world)
  await expectEntry(pane, world, job)

  // No delegate call ever ran, so the ticks must come from the read itself.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the adopted ${job.id}`).toBe(true)
  const state = await headerText(pane, world, job.id)
  expect(FINISHED_WORD.test(state), `the adopted ${job.id} was never read again: ${clip(state)}`).toBe(true)
  expect(noticesNaming(world, job.id), `the finished ${job.id} raised no notice, so the read left the ticks stopped`).toBe(1)
  await pane.unmount()
})

test('M3a-P: an ownerless live record silent past clauth\'s running TTL reads orphaned', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const silentMs = (24 * 60 * 60 + 600) * 1000 + 60_000
  const job = ownJob(world.clock.now(), 'd-mvzfp001-0', 'D1', silentMs, { lastOutputAt: world.clock.now() - silentMs })
  await delegate($, world, job)
  // An older server wrote its record: no owner pid, and its own stamps are
  // the only clock it offers.
  world.put(`${JOBS}/${job.id}.json`, jobFileWith(job, { owner_pid: 0 }))
  await world.clock.advance(TICK_MS)
  const pane = await mountPane($, world)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the silent ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\borphaned\b/.test(text), `the ownerless record silent past the TTL does not read orphaned: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the ownerless corpse still reads running: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\borphaned\b/.test(notices[0]!.text), `the notice for ${job.id} does not say orphaned: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

// --- 12. the main screen, the `/clear` race ---

// The hint line mounted on a surface that reports its layout, as the live
// terminal does on every drawing.
async function mountHintOn($: Engine, isFullscreen: boolean): Promise<Hint> {
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'PromptHint',
    props: { isDraft: false, isWorking: true, hint: ENGINE_HINT },
    viewport: { columns: isFullscreen ? 200 : 110, rows: 50, isFullscreen },
  })
}

test('M3a-R1: on the main screen no unasked open lands, while `/delegates` and the finish toast still work', async ($, on) => {
  installEngineHint(on)
  installCommands(on)
  const world = installWorld(on)
  await startSession($)
  const hint = await mountHintOn($, false)
  await hint.redraw()
  await world.clock.settle()
  const job = ownJob(world.clock.now(), 'd-mvzfr001-0', 'D1', 60_000)
  await delegate($, world, job)
  await world.clock.settle()
  await world.clock.advance(TICK_MS)
  expect(world.opens.length, 'the main screen opened the delegates pane unasked').toBe(0)

  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
  expect(noticesNaming(world, job.id), 'the main screen lost the finish notice').toBe(1)

  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  expect(world.opens.length, '`/delegates` did not open the pane on the main screen').toBe(1)
  await hint.unmount()
})

test('M3a-S: a store read straddling a /clear keeps no finished row of the ended conversation', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  let gate: Promise<void> | null = null
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inRead = new Promise<void>(r => {
    entered = r
  })
  on('fs.read', { path: /\/\.clauth\/jobs\// }, async ($, e, next) => {
    if (gate !== null) {
      const held = gate
      gate = null
      entered()
      await held
    }
    return next(e)
  })
  const stateDrop = installStateDrop(on)
  const world = installWorld(on)
  await startSession($)
  const fin = ownJob(world.clock.now(), 'd-mvzfs001-0', 'D1', 300_000)
  const live = ownJob(world.clock.now(), 'd-mvzfs002-1', 'D2', 200_000)
  await delegate($, world, fin)
  await delegate($, world, live)
  world.job({ ...fin, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  // The next scan is held at a record read while the person's /clear ends the
  // conversation and the engine empties the state the rows live in.
  world.job({ ...live, tail: 'reading the build config again' })
  gate = new Promise<void>(r => {
    release = r
  })
  const ticking = world.clock.advance(TICK_MS)
  await inRead
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()
  release()
  await ticking
  await world.clock.settle()
  // The scan that straddled the reset wrote nothing, so the carried run lands
  // on the next read the `/clear` started.
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  expectUnlisted(tree, fin.id, `the scan that straddled the /clear wrote the ended conversation's finished ${fin.id} into the new one`)
  expect(
    entryFor(tree, live.id) !== undefined,
    `the /clear lost the still live ${live.id}: ${clip(textOf(tree))}`,
  ).toBe(true)
  await pane.unmount()
})

// --- 13. a stale finished record, the cancel reply ---

test('M3a-T: a record already finished before this session started is listed without a notice', async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  const old = ownJob(T0 - 86_000_000, 'd-mvzft001-0', 'D1', 600_000)
  world.job({ ...old, state: 'done', doneAt: T0 - 85_000_000 })
  await startSession($)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  expect(
    noticesNaming(world, old.id),
    `the record finished before the session started raised a notice: ${JSON.stringify(world.notices.map(n => n.text))}`,
  ).toBe(0)
  const pane = await mountPane($, world)
  expect(
    entryFor(await drawn(pane), old.id) !== undefined,
    `the resumed session does not list its finished ${old.id}`,
  ).toBe(true)
  await pane.unmount()
})

test('M3a-U: a background stop clauth answers as asked reads cancelled once its record is gone', async ($, on) => {
  on('mcp.call', { server: 'plugin_clauth_clauth' }, ($, e, next) => {
    if (e.tool !== 'monitor') return next(e)
    const id = String((e.args.job_ids as string[])[0])
    return {
      value: {
        content: [
          {
            type: 'text' as const,
            text: `asked \`${id}\` to stop; each hands back whatever it had produced.\nJob ${id} on D1 (background) — running`,
          },
        ],
        isError: false,
      },
    }
  })
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfu001-0', 'D1', 120_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  await pressStop(pane, world, job.id)
  // clauth's collect lands with no cancelled envelope: the record goes and the
  // ledger stays, as the live run's stopped job left it.
  world.deliver(job.id, job.profile)
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(/\bcancelled\b/.test(text), `the stopped ${job.id} reads done, not cancelled: ${clip(text)}`).toBe(true)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(
    /\bcancelled\b/.test(notices[0]!.text),
    `the notice for ${job.id} does not say cancelled: ${clip(notices[0]!.text)}`,
  ).toBe(true)
  await pane.unmount()
})

// --- 14. the rows write and a pane the engine dropped ---

test('M3a-W: a rows write that never lands leaves the record owed to the next scan', async ($, on) => {
  let refusing = false
  on('state.set', ($, e, next) => {
    if (refusing && e.plugin === 'clauth' && e.key === 'delegates') {
      // The engine's compare-and-set miss: update re-reads and tries again up
      // to its bound, then throws and nothing is written.
      return { value: { isSet: false, version: 0 } }
    }
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfw001-0', 'D1', 60_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)

  refusing = true
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(noticesNaming(world, job.id), 'a notice went out before the rows were stored').toBe(0)

  refusing = false
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(
    noticesNaming(world, job.id),
    `the scan marked the record read before its rows landed, so the finished ${job.id} was never noticed`,
  ).toBe(1)
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost ${job.id}`).toBe(true)
  expect(FINISHED_WORD.test(await headerText(pane, world, job.id)), `the header for ${job.id} never settled done`).toBe(true)
  await pane.unmount()
})

test('M3a-X: a view whose pane no longer draws is not read', async ($, on) => {
  const tails: string[][] = []
  on('process.run', { argv: [/^tail$/] }, ($, e, next) => {
    tails.push([...e.argv])
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const cwd = '/work/probe'
  const job = ownJob(world.clock.now(), 'd-mvzfx001-0', 'D1', 300_000, { sessionId: 'sess-unload', cwd })
  const file = transcriptPath(cwd, 'sess-unload')
  world.put(file, transcriptFile('unload', 'sess-unload', cwd))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  pane = await openEntry($, world, pane, job.id)
  expect(tails.length > 0, 'the open view read no transcript').toBe(true)
  await pane.unmount()

  // A pane the engine drops as `unload` (its drawing threw) leaves the pane
  // list without running the closer hook, so the view it opened stays set
  // with nothing drawing it; the transcript goes on growing.
  world.open.delete(world.opens.at(-1)!.id)
  world.put(
    file,
    `${transcriptFile('unload', 'sess-unload', cwd)}${JSON.stringify({
      type: 'assistant',
      message: { role: 'assistant', content: [{ type: 'text', text: 'unload-after-drop' }] },
      sessionId: 'sess-unload',
      cwd,
    })}\n`,
  )
  const before = tails.length
  await world.clock.advance(3 * TICK_MS)
  expect(
    tails.length - before,
    `the ticks read a transcript no open pane draws: ${tails.length - before} reads`,
  ).toBe(0)
})

// --- 15. the session epoch, the stop's replies ---

test('M3a-Y5: a store read straddling a /resume writes nothing into the resumed conversation', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  let gate: Promise<void> | null = null
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inRead = new Promise<void>(r => {
    entered = r
  })
  on('fs.read', { path: /\/\.clauth\/jobs\// }, async ($, e, next) => {
    if (gate !== null) {
      const held = gate
      gate = null
      entered()
      await held
    }
    return next(e)
  })
  const stateDrop = installStateDrop(on)
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzy0005-0', 'D2', 200_000)
  await delegate($, world, job)
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  // The next scan is held at a record read while the person's /resume ends
  // the conversation and the engine empties the state the rows live in.
  world.job({ ...job, tail: 'reading the build config again' })
  gate = new Promise<void>(r => {
    release = r
  })
  const ticking = world.clock.advance(TICK_MS)
  await inRead
  sid = 'sid-resumed'
  await $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()
  release()
  await ticking
  await world.clock.settle()

  // The run finishes; the resumed conversation must not hear of it.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(3 * TICK_MS)
  await world.clock.settle()
  const pane = await $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'Pane',
    requestId: 'clauth-delegates',
    props: { ...PANE_PROPS, title: 'clauth delegates' },
  })
  expectUnlisted(await drawn(pane), job.id, `the scan that straddled the /resume listed the ended conversation's ${job.id} in the resumed one`)
  expect(noticesNaming(world, job.id), `a notice named ${job.id} in the conversation the /resume started`).toBe(0)
  await pane.unmount()
})

test('M3a-Y6: a stop on a blocking row clauth asks to stop raises no failure toast', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const keeper = ownJob(world.clock.now(), 'd-mvzy0006-0', 'D2', 30_000)
  await delegate($, world, keeper)
  const blocking = ownJob(world.clock.now(), 'd-mvzy0007-1', 'D1', 120_000, { sessionId: 'sess-blocking-stop' })
  world.put(`${JOBS}/${blocking.id}.live.json`, jobFile(blocking))
  world.blockingIds.add(blocking.id)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  await pressStop(pane, world, blocking.id)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(blocking.id)).length,
    `stopping the blocking ${blocking.id} did not ask clauth's monitor`,
  ).toBe(1)
  expect(noticesNaming(world, blocking.id), 'a stop clauth answered as asked raised a failure notice').toBe(0)
  await pane.unmount()
})

test('M3a-Y7: a stop pressed after a background run finished collects nothing', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzy0008-0', 'D1', 120_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)

  // The run finished; no scan has read its done record yet, so its row still
  // offers the stop.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await pressStop(pane, world, job.id)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(job.id)).length,
    `the stop collected the finished ${job.id}, so its result went to the mod instead of the model`,
  ).toBe(0)

  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the finished ${job.id}`).toBe(true)
  const state = await headerText(pane, world, job.id)
  expect(FINISHED_WORD.test(state), `the finished ${job.id} does not read done: ${clip(state)}`).toBe(true)
  expect(noticesNaming(world, job.id), `the finished ${job.id} was not noticed exactly once`).toBe(1)
  await pane.unmount()
})

test('M3a-Y8: a stop clauth answers already finished leaves the row done', async ($, on) => {
  on('mcp.call', { server: 'plugin_clauth_clauth' }, ($, e, next) => {
    if (e.tool !== 'monitor') return next(e)
    const id = String((e.args.job_ids as string[])[0])
    return { value: { content: [{ type: 'text' as const, text: `already finished: \`${id}\`.` }], isError: false } }
  })
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzy0009-0', 'D1', 120_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  await pressStop(pane, world, job.id)
  world.deliver(job.id, job.profile)
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })

  expect(entryFor(await drawn(pane), job.id) !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = await headerText(pane, world, job.id)
  expect(FINISHED_WORD.test(text), `the already-finished ${job.id} does not read done: ${clip(text)}`).toBe(true)
  expect(/\bcancelled\b/.test(text), `a reply that did not ask to stop marked ${job.id} cancelled: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\bcancelled\b/.test(notices[0]!.text), `the notice for ${job.id} says cancelled: ${clip(notices[0]!.text)}`).toBe(false)
  await pane.unmount()
})

test('M3a-Y9: a pane open that fails does not drop the scan\'s finish notice', async ($, on) => {
  on('ui.open', { id: 'clauth-delegates' }, () => {
    throw new Error('open refused')
  })
  let release: () => void = () => {}
  const held = new Promise<void>(r => {
    release = r
  })
  on('tool.call', { tool: DELEGATE }, async ($, e, next) => {
    await held
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const live = ownJob(world.clock.now(), 'd-mvzy0010-0', 'D2', 30_000)
  world.job(live)
  const fin = ownJob(world.clock.now(), 'd-mvzy0011-1', 'D1', 90_000)
  world.job({ ...fin, state: 'done', doneAt: world.clock.now() })
  const call = $.tool.call({ tool: DELEGATE, profile: 'D2', prompt: 'hold', background: false })
  await world.clock.settle()
  await world.clock.advance(TICK_MS)
  expect(noticesNaming(world, fin.id), 'the failed pane open dropped the finished run\'s notice').toBe(1)
  release()
  await call
  await world.clock.settle()
})

test('M3a-Y10: a store read landing while session.start registers cannot toast an old record', async ($, on) => {
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inRegister = new Promise<void>(r => {
    entered = r
  })
  const held = new Promise<void>(r => {
    release = r
  })
  on('command.register', { name: 'delegates' }, async ($, e, next) => {
    entered()
    await held
    return next(e)
  })
  const lists: string[] = []
  on('fs.list', { path: JOBS }, ($, e, next) => {
    lists.push(e.path)
    return next(e)
  })
  const world = installWorld(on)
  const old = ownJob(T0 - 86_000_000, 'd-mvzy0012-0', 'D1', 600_000)
  world.job({ ...old, state: 'done', doneAt: T0 - 85_000_000 })

  const starting = $.session.start({ cwd: HOST_CWD, surface: 'terminal', isInteractive: true })
  await inRegister
  // The surface draws once while the start is still in flight, and the draw
  // asks for a store read on the next clock tick.
  const band = await mountAbovePrompt($)
  await band.redraw()
  await world.clock.settle()
  const listsInWindow = lists.length
  expect(
    noticesNaming(world, old.id),
    `a store read during session.start toasted ${old.id}, which ended before this process started`,
  ).toBe(0)
  // A draw that asked for no read would keep the toast away just as well.
  expect(listsInWindow, 'the band drawn during session.start listed no store, so no read ran in the window').toBeGreaterThan(0)

  release()
  await starting
  await world.clock.settle()
  await band.unmount()
})

// --- 16. a store read inside `session.end`, the Esc hand-off, the epoch guards ---

// The bridge `end` post a session.end awaits, held until released, so a test
// can work inside the end's own window.
function holdEndPost(on: On) {
  let armed = false
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inEnd = new Promise<void>(r => {
    entered = r
  })
  const gate = new Promise<void>(r => {
    release = r
  })
  on('http.fetch', { url: /\/event$/ }, async ($, e, next) => {
    if (armed && typeof e.init?.body === 'string' && e.init.body.includes('"kind":"end"')) {
      armed = false
      entered()
      await gate
    }
    return next(e)
  })
  return {
    arm: () => {
      armed = true
    },
    inEnd,
    release: () => release(),
  }
}

// The next job-store file read, held until released, so a test can end the
// session with a store read in flight.
function holdJobRead(on: On) {
  let gate: Promise<void> | null = null
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inRead = new Promise<void>(r => {
    entered = r
  })
  on('fs.read', { path: /\/\.clauth\/jobs\// }, async ($, e, next) => {
    if (gate !== null) {
      const held = gate
      gate = null
      entered()
      await held
    }
    return next(e)
  })
  return {
    arm: () => {
      gate = new Promise<void>(r => {
        release = r
      })
    },
    get inRead() {
      return inRead
    },
    release: () => release(),
  }
}

// The delegates rows as they stood when the first read was asked for, held
// until released: the reset can land while the read is in flight.
function holdRowsRead(on: On) {
  let gate: Promise<void> | null = null
  let release: () => void = () => {}
  let entered: () => void = () => {}
  const inRead = new Promise<void>(r => {
    entered = r
  })
  on('state.get', { plugin: 'clauth', key: 'delegates' }, async ($, e, next) => {
    if (gate === null) return next(e)
    const read = await next(e)
    const held = gate
    gate = null
    entered()
    await held
    return read
  })
  return {
    arm: () => {
      gate = new Promise<void>(r => {
        release = r
      })
    },
    get inRead() {
      return inRead
    },
    release: () => release(),
  }
}

// How long a window test waits for a read the end must not start.
function noReadWindow(): Promise<void> {
  return new Promise<void>(r => setTimeout(r, 50))
}

for (const scanInWindow of [true, false]) {
  test(`M3a-Z1: a store read started inside a /clear's end keeps the carried live run (scanInWindow=${scanInWindow})`, async ($, on) => {
    let sid = OWN
    on('session.id', {}, () => ({ value: sid }))
    const endHold = holdEndPost(on)
    const readHold = holdJobRead(on)
    const stateDrop = installStateDrop(on)
    const world = installWorld(on)
    await startSession($)
    const fin = ownJob(world.clock.now(), 'd-mvzfz001-0', 'D1', 300_000)
    const live = ownJob(world.clock.now(), 'd-mvzfz002-1', 'D2', 200_000)
    await delegate($, world, fin)
    await delegate($, world, live)
    world.job({ ...fin, state: 'done', doneAt: world.clock.now() })
    await world.clock.advance(TICK_MS)
    await world.clock.settle()

    // The /clear's end awaits its bridge post; a tick fires in that await,
    // after the carry and before the engine drops the state.
    endHold.arm()
    const ending = $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
    await endHold.inEnd
    let ticking: Promise<unknown> = Promise.resolve()
    if (scanInWindow) {
      readHold.arm()
      ticking = world.clock.advance(TICK_MS)
      await Promise.race([readHold.inRead, noReadWindow()])
    }
    endHold.release()
    await ending
    sid = 'sid-after-clear'
    stateDrop.drop()
    readHold.release()
    await ticking
    await world.clock.settle()
    await world.clock.advance(2 * TICK_MS)
    await world.clock.settle()

    const pane = await mountPane($, world)
    const tree = await drawn(pane)
    expect(
      entryFor(tree, live.id) !== undefined,
      `the /clear lost the live ${live.id} the read inside its end carried away: ${clip(textOf(tree))}`,
    ).toBe(true)
    expectUnlisted(tree, fin.id, `a read inside the /clear listed the ended conversation's finished ${fin.id}`)
    await pane.unmount()
  })
}

for (const scanInWindow of [true, false]) {
  test(`M3a-Z2: a store read started inside a /resume's end lists nothing in the resumed conversation (scanInWindow=${scanInWindow})`, async ($, on) => {
    let sid = OWN
    on('session.id', {}, () => ({ value: sid }))
    const endHold = holdEndPost(on)
    const readHold = holdJobRead(on)
    const stateDrop = installStateDrop(on)
    const world = installWorld(on)
    await startSession($)
    const job = ownJob(world.clock.now(), 'd-mvzfz003-0', 'D2', 200_000)
    await delegate($, world, job)
    await world.clock.advance(TICK_MS)
    await world.clock.settle()

    // The /resume's end stops the ticks and awaits its post; the redrawn band
    // asks for a read meanwhile.
    world.job({ ...job, tail: 'reading the build config again' })
    endHold.arm()
    const ending = $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: OWN } })
    await endHold.inEnd
    let ticking: Promise<unknown> = Promise.resolve()
    if (scanInWindow) {
      const band = await mountAbovePrompt($)
      await band.redraw()
      await band.unmount()
      readHold.arm()
      ticking = world.clock.advance(0)
      await Promise.race([readHold.inRead, noReadWindow()])
    }
    endHold.release()
    await ending
    sid = 'sid-resumed'
    stateDrop.drop()
    readHold.release()
    await ticking
    await world.clock.settle()
    world.job({ ...job, state: 'done', doneAt: world.clock.now() })
    await world.clock.advance(3 * TICK_MS)
    await world.clock.settle()

    const pane = await mountPane($, world)
    expectUnlisted(await drawn(pane), job.id, `a read inside the /resume listed the ended conversation's ${job.id} in the resumed one`)
    expect(
      noticesNaming(world, job.id),
      `a read inside the /resume noticed ${job.id} in the conversation it left`,
    ).toBe(0)
    await pane.unmount()
  })
}

for (const scanInWindow of [true, false]) {
  test(`M3a-Z3: a store read started inside a /clear's end keeps the ended conversation's finished row out (scanInWindow=${scanInWindow})`, async ($, on) => {
    let sid = OWN
    on('session.id', {}, () => ({ value: sid }))
    const endHold = holdEndPost(on)
    const readHold = holdJobRead(on)
    const stateDrop = installStateDrop(on)
    const world = installWorld(on)
    await startSession($)
    const fin = ownJob(world.clock.now(), 'd-mvzfz004-0', 'D1', 300_000)
    const live = ownJob(world.clock.now(), 'd-mvzfz005-1', 'D2', 200_000)
    await delegate($, world, fin)
    await delegate($, world, live)
    world.job({ ...fin, state: 'done', doneAt: world.clock.now() })
    await world.clock.advance(TICK_MS)
    await world.clock.settle()

    // As above, but the live run's record moves since its last read, so the
    // read inside the end has a changed row to fold into the old ones.
    world.job({ ...live, tail: 'reading the build config again' })
    endHold.arm()
    const ending = $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
    await endHold.inEnd
    let ticking: Promise<unknown> = Promise.resolve()
    if (scanInWindow) {
      readHold.arm()
      ticking = world.clock.advance(TICK_MS)
      await Promise.race([readHold.inRead, noReadWindow()])
    }
    endHold.release()
    await ending
    sid = 'sid-after-clear'
    stateDrop.drop()
    readHold.release()
    await ticking
    await world.clock.settle()
    await world.clock.advance(2 * TICK_MS)
    await world.clock.settle()

    const pane = await mountPane($, world)
    const tree = await drawn(pane)
    expectUnlisted(tree, fin.id, `a read inside the /clear wrote the ended conversation's finished ${fin.id} into the new one`)
    expect(
      entryFor(tree, live.id) !== undefined,
      `the /clear lost the still live ${live.id}: ${clip(textOf(tree))}`,
    ).toBe(true)
    await pane.unmount()
  })
}

for (const handedOff of [true, false]) {
  test(`M3a-Z4: a stop on a blocking row whose run Esc handed off collects nothing (handedOff=${handedOff})`, async ($, on) => {
    const world = installWorld(on)
    await startSession($)
    const keeper = ownJob(world.clock.now(), 'd-mvzfz006-0', 'D2', 30_000)
    await delegate($, world, keeper)
    const blocking = ownJob(world.clock.now(), 'd-mvzfz007-1', 'D1', 120_000)
    world.put(`${JOBS}/${blocking.id}.live.json`, jobFile(blocking))
    await world.clock.advance(POLL_MS)
    const pane = await mountPane($, world)
    expect(
      /\bblocking\b/.test(await headerText(pane, world, blocking.id)),
      `the pane draws no blocking header for ${blocking.id}`,
    ).toBe(true)

    // Esc hands the blocking run off to a job file, and the run lands before
    // the next scan reads that file: the row still offers its stop.
    if (handedOff) {
      world.remove(`${JOBS}/${blocking.id}.live.json`)
      world.job({ ...blocking, state: 'done', doneAt: world.clock.now() })
    }
    await pressStop(pane, world, blocking.id)
    expect(
      world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(blocking.id)).length,
      handedOff
        ? `the stop on the handed-off ${blocking.id} asked clauth's monitor to collect its finished record`
        : `the stop on the still-running blocking ${blocking.id} never reached clauth's monitor`,
    ).toBe(handedOff ? 0 : 1)
    expect(
      world.files.has(`${JOBS}/${blocking.id}.json.delivered`),
      `the stop on the handed-off ${blocking.id} let clauth deliver the run the mod dropped`,
    ).toBe(false)
    await pane.unmount()
  })
}

test('M3a-Z5: a store read that took the carried rows before a /clear still leaves the run listed', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const rowsHold = holdRowsRead(on)
  const stateDrop = installStateDrop(on)
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfz008-0', 'D1', 300_000)
  await delegate($, world, job)
  // One tick reads the record and stores its stamp before the clear.
  await world.clock.advance(POLL_MS)
  await world.clock.settle()

  // The next read answers with the rows as they stood before the /clear and
  // lands after it; the record does not move, so its rows compare equal.
  rowsHold.arm()
  const ticking = world.clock.advance(TICK_MS)
  await rowsHold.inRead
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()
  rowsHold.release()
  await ticking
  await world.clock.settle()
  await world.clock.advance(2 * TICK_MS)
  await world.clock.settle()

  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  expect(
    entryFor(tree, job.id) !== undefined,
    `the read that straddled the /clear lost the still live ${job.id}: ${clip(textOf(tree))}`,
  ).toBe(true)
  await pane.unmount()
})

test('M3a-Z6: a rows write the /resume lands under is not written over', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const stateDrop = installStateDrop(on)
  let armed = false
  let releaseSet: () => void = () => {}
  let setEntered: () => void = () => {}
  const inSet = new Promise<void>(r => {
    setEntered = r
  })
  const setGate = new Promise<void>(r => {
    releaseSet = r
  })
  on('state.set', { plugin: 'clauth', key: 'delegates' }, async ($, e, next) => {
    if (!armed) return next(e)
    armed = false
    // The reset lands between the rows read and their write: the write that
    // raced it misses, and the retry re-reads the epoch.
    setEntered()
    await setGate
    return { value: { isSet: false, version: 0 } }
  })
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfz009-0', 'D2', 200_000)
  await delegate($, world, job)
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  // The record moves, so the next read has rows to write, and the reset lands
  // inside that write.
  world.job({ ...job, tail: 'reading the build config again' })
  armed = true
  const ticking = world.clock.advance(TICK_MS)
  await inSet
  sid = 'sid-resumed'
  await $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()
  releaseSet()
  await ticking
  await world.clock.settle()
  await world.clock.advance(2 * TICK_MS)
  await world.clock.settle()

  const pane = await mountPane($, world)
  expectUnlisted(await drawn(pane), job.id, `the write the /resume landed under put the ended conversation's ${job.id} into the resumed one`)
  expect(noticesNaming(world, job.id), `the write the /resume landed under noticed ${job.id}`).toBe(0)
  await pane.unmount()
})

// --- 17. a /resume retires the left conversation's state, and the guards behind the /clear carry ---

for (const clearFirst of [true, false]) {
  test(`M3a-AB1: a /resume before a /clear's carry commits lists none of its run in the resumed conversation (clearFirst=${clearFirst})`, async ($, on) => {
    let sid = OWN
    on('session.id', {}, () => ({ value: sid }))
    const stateDrop = installStateDrop(on)
    const world = installWorld(on)
    await startSession($)
    const job = ownJob(world.clock.now(), 'd-mvzga001-0', 'D2', 200_000)
    await delegate($, world, job)
    await world.clock.advance(TICK_MS)
    await world.clock.settle()
    let left = OWN
    if (clearFirst) {
      // The /clear carries the live row; the /resume follows before any read
      // takes it.
      await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
      sid = 'sid-after-clear'
      left = sid
      stateDrop.drop()
    }
    await $.session.end({ reason: 'resume', sessionId: left, resume: { id: 'sid-B' } })
    sid = 'sid-B'
    stateDrop.drop()
    await $.command.run({ command: 'delegates' })
    await world.clock.settle()
    const pane = await mountPane($, world)
    expectUnlisted(await drawn(pane), job.id, `the /resume listed the ${left} conversation's carried ${job.id} in the resumed one`)
    await pane.unmount()
  })
}

for (const clearFirst of [true, false]) {
  test(`M3a-AB2: a /resume drops the ids a /clear ended, so its still-running run is not adopted again (clearFirst=${clearFirst})`, async ($, on) => {
    let sid = OWN
    on('session.id', {}, () => ({ value: sid }))
    const stateDrop = installStateDrop(on)
    const world = installWorld(on)
    await startSession($)
    const job = ownJob(world.clock.now(), 'd-mvzga002-0', 'D2', 200_000)
    await delegate($, world, job)
    await world.clock.advance(TICK_MS)
    await world.clock.settle()
    let left = OWN
    if (clearFirst) {
      // The /clear's ticks read the run again and store it under the id the
      // /resume then leaves.
      await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
      sid = 'sid-after-clear'
      left = sid
      stateDrop.drop()
      await world.clock.advance(3 * TICK_MS)
      await world.clock.settle()
    }
    await $.session.end({ reason: 'resume', sessionId: left, resume: { id: 'sid-B' } })
    sid = 'sid-B'
    stateDrop.drop()
    // The run's record moves while it still runs, so a read of it is offered.
    world.job({ ...job, tail: 'reading the build config again' })
    await world.clock.advance(TICK_MS)
    await world.clock.settle()
    await $.command.run({ command: 'delegates' })
    await world.clock.settle()
    const pane = await mountPane($, world)
    expectUnlisted(await drawn(pane), job.id, `the /resume adopted the ${left} conversation's running ${job.id}`)
    world.job({ ...job, state: 'done', doneAt: world.clock.now() })
    await world.clock.advance(3 * TICK_MS)
    await world.clock.settle()
    expectUnlisted(await drawn(pane), job.id, `the /resume listed the ${left} conversation's finished ${job.id}`)
    expect(noticesNaming(world, job.id), `the /resume noticed ${job.id} in the conversation it left`).toBe(0)
    await pane.unmount()
  })
}

for (const mode of ['stamped', 'fresh'] as const) {
  for (const word of ['done', 'running'] as const) {
    test(`M3a-AB3: a /resume re-reads the resumed conversation's own record stamped before it (mode=${mode}, word=${word})`, async ($, on) => {
      const B = 'sid-B'
      let sid = mode === 'fresh' ? B : OWN
      on('session.id', {}, () => ({ value: sid }))
      const stateDrop = installStateDrop(on)
      const world = installWorld(on)
      const base = ownJob(world.clock.now(), 'd-mvzga003-0', 'D3', 3_600_000, { hostSession: B })
      const bJob: Job = word === 'done' ? { ...base, state: 'done', doneAt: world.clock.now() - 1_800_000 } : base
      world.job(bJob)
      await startSession($)
      if (mode === 'stamped') {
        // Conversation A reads the store; the /resume then moves the process
        // to B, whose record has not moved since that read.
        const own = ownJob(world.clock.now(), 'd-mvzga004-0', 'D1', 30_000)
        await delegate($, world, own)
        world.job({ ...own, state: 'done', doneAt: world.clock.now() })
        await world.clock.advance(POLL_MS)
        await world.clock.settle()
        await $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: B } })
        sid = B
        stateDrop.drop()
      }
      await world.clock.advance(POLL_MS)
      await world.clock.settle()
      await $.command.run({ command: 'delegates' })
      await world.clock.settle()
      const pane = await mountPane($, world)
      const tree = await drawn(pane)
      expect(
        entryFor(tree, bJob.id) !== undefined,
        `the /resume never read the ${word} ${bJob.id} stamped before the end: ${clip(textOf(tree))}`,
      ).toBe(true)
      expect(noticesNaming(world, bJob.id), `the /resume noticed ${bJob.id} again in the conversation it entered`).toBe(0)
      await pane.unmount()
    })
  }
}

test('M3a-AB4: a record that moved since its stamp is listed in the resumed conversation', async ($, on) => {
  const B = 'sid-B'
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const stateDrop = installStateDrop(on)
  const world = installWorld(on)
  const bJob = ownJob(world.clock.now(), 'd-mvzga005-0', 'D3', 3_600_000, { hostSession: B })
  world.job(bJob)
  await startSession($)
  const own = ownJob(world.clock.now(), 'd-mvzga006-0', 'D1', 30_000)
  await delegate($, world, own)
  world.job({ ...own, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
  await world.clock.settle()
  await $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: B } })
  sid = B
  stateDrop.drop()
  world.job({ ...bJob, tail: 'reading the build config again' })
  await world.clock.advance(POLL_MS)
  await world.clock.settle()
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  expect(
    entryFor(tree, bJob.id) !== undefined,
    `the /resume left out the moved ${bJob.id} of the conversation it entered: ${clip(textOf(tree))}`,
  ).toBe(true)
  expectUnlisted(tree, own.id, `the /resume listed the left conversation's finished ${own.id}`)
  await pane.unmount()
})

test('M3a-AB5: a store read fired inside a /clear\'s end reaches no store, and the carried run stays listed', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const endHold = holdEndPost(on)
  const stateDrop = installStateDrop(on)
  let reads = 0
  on('fs.list', {}, async ($, e, next) => {
    reads += 1
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const fin = ownJob(world.clock.now(), 'd-mvzga007-0', 'D1', 300_000)
  const live = ownJob(world.clock.now(), 'd-mvzga008-1', 'D2', 200_000)
  await delegate($, world, fin)
  await delegate($, world, live)
  world.job({ ...fin, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  // The /clear's end awaits its bridge post; a tick fires in that await, and
  // its read must stop at the end's entry guard before it lists the store.
  endHold.arm()
  const ending = $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  await endHold.inEnd
  const before = reads
  const ticking = world.clock.advance(TICK_MS)
  await new Promise<void>(r => setTimeout(r, 100))
  const readsInWindow = reads - before
  endHold.release()
  await ending
  sid = 'sid-after-clear'
  stateDrop.drop()
  await ticking
  await world.clock.settle()
  await world.clock.advance(2 * TICK_MS)
  await world.clock.settle()
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()

  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  expect(readsInWindow, 'a store read started inside the /clear\'s end and listed the store').toBe(0)
  expect(
    entryFor(tree, live.id) !== undefined,
    `the read inside the /clear lost the carried live ${live.id}: ${clip(textOf(tree))}`,
  ).toBe(true)
  await pane.unmount()
})

test('M3a-AB6: a rows write that fails after a /clear keeps the carried run for the retry', async ($, on) => {
  let sid = OWN
  on('session.id', {}, () => ({ value: sid }))
  const stateDrop = installStateDrop(on)
  let refusing = false
  on('state.set', { plugin: 'clauth', key: 'delegates' }, ($, e, next) => {
    if (refusing) {
      // The engine's compare-and-set miss: update re-reads and tries again up
      // to its bound, then throws and nothing is written.
      return { value: { isSet: false, version: 0 } }
    }
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzga009-0', 'D2', 200_000)
  await delegate($, world, job)
  await world.clock.advance(TICK_MS)
  await world.clock.settle()

  // The /clear carries the live row, and the first read after it fails to
  // store its rows. That tick stops the ticks, so the retry comes from the
  // read a person's open asks for.
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  sid = 'sid-after-clear'
  stateDrop.drop()
  refusing = true
  await world.clock.advance(TICK_MS)
  refusing = false
  // The write never landed, so the engine's state is still the emptied one the
  // /clear left: the kit ends its drop on a write attempt, not a landed one.
  stateDrop.drop()
  // The run finishes before the retry: a finished record of the ended
  // conversation is never adopted again, so only the kept carry can list it.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()

  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  expect(
    entryFor(tree, job.id) !== undefined,
    `the failed write lost the carried ${job.id} before its retry: ${clip(textOf(tree))}`,
  ).toBe(true)
  const state = await headerText(pane, world, job.id)
  expect(
    FINISHED_WORD.test(state),
    `the carried ${job.id} never settled done on the retry: ${clip(state)}`,
  ).toBe(true)
  expect(noticesNaming(world, job.id), `the carried ${job.id} finished on the retry without one notice`).toBe(1)
  await pane.unmount()
})

// The seam with the held-question bridge: a `delegate` call can carry a
// permission ask the phone does not hold, which the generic `tool.call` hook
// closes once the call settles, around the delegate hook's own work.
type BridgeEvent = Record<string, unknown>

function recordBridgeEvents(on: On): BridgeEvent[] {
  const events: BridgeEvent[] = []
  on('http.fetch', { url: /\/event$/ }, async ($, e, next) => {
    events.push(typeof e.init?.body === 'string' ? (JSON.parse(e.init.body) as BridgeEvent) : {})
    return next(e)
  })
  return events
}

// Every desk dialog waits until the test answers it.
function deskDialogs(on: On): Array<(behavior: 'allow' | 'deny') => void> {
  const answers: Array<(behavior: 'allow' | 'deny') => void> = []
  on('classic.PermissionRequest', async () => {
    const behavior = await new Promise<'allow' | 'deny'>(answer => answers.push(answer))
    return { decision: behavior === 'allow' ? { behavior } : { behavior, message: 'no' } }
  })
  return answers
}

const asksOf = (events: BridgeEvent[], kind: 'ask_opened' | 'ask_closed'): BridgeEvent[] =>
  events.filter(event => event.kind === kind)

test('M3a-AC1: an unheld ask on a delegate call closes once by the desk and no pane opens by itself', async ($, on) => {
  const events = recordBridgeEvents(on)
  on('tool.check', ($, e, next) => (e.tool === DELEGATE ? { decision: 'ask' as const } : next(e)))
  const dialogs = deskDialogs(on)
  // clauth writes the record only once the permitted call runs, so no tick
  // while the dialog is up can find the run.
  on('tool.call', { tool: DELEGATE }, ($, e, next) => {
    world.job(ownJob(world.clock.now(), 'd-mvzac001-0', 'D1', 1_000))
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  const call = $.tool.call({ tool: DELEGATE, profile: 'D1', prompt: 'run the probe', background: true })
  await world.clock.settle()
  expect(dialogs, 'the delegate call raised no desk dialog').toHaveLength(1)
  expect(asksOf(events, 'ask_closed'), 'the ask closed before the desk answered').toHaveLength(0)
  dialogs[0]!('allow')
  await call
  await world.clock.settle()

  const opened = asksOf(events, 'ask_opened')
  expect(opened, 'the ask was not opened exactly once').toHaveLength(1)
  expect(
    asksOf(events, 'ask_closed').map(event => [event.ask, event.by, event.session]),
    'the ask did not close once, by the desk, under its own session',
  ).toEqual([[opened[0]!.ask, 'desk', OWN]])
  expect(world.opens.length, 'the delegate call opened the delegates pane by itself').toBe(0)
})

test('M3a-AC2: a pane lookup that fails before the delegate call runs still lets its unheld ask close', async ($, on) => {
  const events = recordBridgeEvents(on)
  on('tool.check', ($, e, next) => (e.tool === DELEGATE ? { decision: 'ask' as const } : next(e)))
  const dialogs = deskDialogs(on)
  let refusePanes = false
  on('ui.panes', {}, ($, e, next) => {
    if (refusePanes) throw new Error('panes refused')
    return next(e)
  })
  const world = installWorld(on)
  await startSession($)
  refusePanes = true
  const call = $.tool.call({ tool: DELEGATE, profile: 'D1', prompt: 'run the probe', background: true })
  await world.clock.settle()
  for (const answer of dialogs) answer('allow')
  await call
  await world.clock.settle()
  refusePanes = false

  expect(asksOf(events, 'ask_opened'), 'the ask was not opened exactly once').toHaveLength(1)
  expect(asksOf(events, 'ask_closed'), 'a failed pane lookup left the unheld ask open').toHaveLength(1)
})

// --- 18. the band above the prompt and the split pane ---
//
// The band:
//   running:   ● [ 3 delegates ]   ✻ D1 5m  ✓ zcode 33m  ✢ D4 35m
//   all done:  ● [ 3 delegates ] done   (faint whole)
// The running row holds the current batch: every delegate since the row last
// went all-done, finished ones included. The all-done line counts every
// delegate of the session, and its count opens the pane. These tests pin the
// glyphs, colours, counts and order, not the cells between them.

// The spending mark Claude Code cycles while it works, and the per-state marks.
const RUNNING_MARKS: readonly string[] = ['·', '✢', '*', '✶', '✻', '✽']
const DONE_MARK = '✓'
const FAILED_MARK = '✗'
const STOPPED_MARK = '■'
const LEAD_DOT = '●'
// clauth's accent (`theme::accent_color`) and the pane's background (`theme::bg()`).
const ACCOUNT_BLUE = '#43abe5'
const PANE_BG = '#1e1e2e'
// A once-a-second sample cadence for the band's frames.
const BLINK_MS = 1_000

const BAND_PROPS = {
  hasSurvey: false,
  isCollapsed: false,
  isWorking: true,
  maxRows: 12,
  bodyColumns: 160,
  scroll: { offset: 0, bodyRows: 12 },
  view: {},
}

type Band = Mounted<'terminal', 'AbovePrompt'>

async function mountBand($: Engine, bodyColumns = BAND_PROPS.bodyColumns): Promise<Band> {
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'AbovePrompt',
    props: { ...BAND_PROPS, bodyColumns },
    viewport: { columns: bodyColumns + 5, rows: 50, isFullscreen: true },
  })
}

async function bandFrame(band: Band): Promise<unknown> {
  await band.redraw()
  return band.drawn()
}

// The Text leaves whose whole text is one of `glyphs`, in document order.
function glyphLeaves(root: unknown, glyphs: readonly string[]): Drawn[] {
  return elements(root).filter(el => el.type === 'Text' && glyphs.includes(textOf(el).trim()))
}

// The innermost Text element whose text holds `word` as a whole token.
function leafWith(root: unknown, word: string): Drawn | undefined {
  return elements(root)
    .filter(el => el.type === 'Text' && hasWord(textOf(el), word))
    .at(-1)
}

// One delegate's chip in the band: the Button naming its account and no
// other fixture account.
function chipFor(root: unknown, account: string, accounts: readonly string[]): Drawn | undefined {
  return buttonsIn(root).find(b => {
    const text = textOf(b)
    return hasWord(text, account) && accounts.every(other => other === account || !hasWord(text, other))
  })
}

// The band's count control: the Button carrying `N delegate(s)`.
function countControl(root: unknown): Drawn | undefined {
  return buttonsIn(root).find(b => /\b\d+ delegates?\b/.test(textOf(b)))
}

// The colour an element draws in: its own `color`, else its nearest
// ancestor's.
function colourOf(root: unknown, target: Drawn): string | undefined {
  const walk = (node: unknown, inherited: string | undefined): string | undefined => {
    if (!isDrawn(node)) return undefined
    const own = typeof node.props?.color === 'string' ? (node.props.color as string) : inherited
    if (node === target) return own ?? ''
    for (const child of node.children ?? []) {
      const found = walk(child, own)
      if (found !== undefined) return found
    }
    return undefined
  }
  const found = walk(root, undefined)
  return found === '' ? undefined : found
}

// Whether an element or one of its ancestors dims it.
function isDim(root: unknown, target: Drawn): boolean {
  const walk = (node: unknown, dim: boolean): boolean | undefined => {
    if (!isDrawn(node)) return undefined
    const own = dim || node.props?.dimColor === true
    if (node === target) return own
    for (const child of node.children ?? []) {
      const found = walk(child, own)
      if (found !== undefined) return found
    }
    return undefined
  }
  return walk(root, false) ?? false
}

// The frames a band draws `count` times, `stepMs` apart on the mocked clock.
async function bandFrames(band: Band, world: World, count: number, stepMs: number): Promise<unknown[]> {
  const frames: unknown[] = []
  for (let i = 0; i < count; i += 1) {
    if (i > 0) await world.clock.advance(stepMs)
    frames.push(await bandFrame(band))
  }
  return frames
}

const docIndex = (root: unknown, el: Drawn): number => elements(root).indexOf(el)

// A running band of three: D1 started 5 min ago, zcode 33 min ago and
// finished, D4 35 min ago.
function bandBatch(now: number): Job[] {
  return [
    ownJob(now, 'd-mvzc1aaa-0', 'D1', 300_000),
    ownJob(now, 'd-mvzc1bbb-1', 'zcode', 1_980_000, { state: 'done', doneAt: now - 60_000 }),
    ownJob(now, 'd-mvzc1ccc-2', 'D4', 2_100_000),
  ]
}

test('M3c-1: a running batch draws one band row, a line below the transcript: the lead dot, the count control, then a chip per delegate newest first', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const jobs = bandBatch(world.clock.now())
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const accounts = jobs.map(j => j.profile)
  const band = await mountBand($)
  const frames = await bandFrames(band, world, 2, BLINK_MS)
  const tree = frames[0]!

  // The kit stacks every plugin's band row in a column, each tagged with its
  // plugin, so the margin is read off the mod's own row.
  const row = elements(tree).find(el => (el as { part?: { plugin?: string } }).part?.plugin === 'clauth')
  expect(row !== undefined && row.props?.marginTop === 1, `the band row has no one-line top margin: ${clip(JSON.stringify(tree) ?? '')}`).toBe(true)
  expect(
    frames.some(frame => glyphLeaves(frame, [LEAD_DOT]).length === 1),
    `no frame of a running batch draws the lead ${LEAD_DOT}: ${clip(textOf(tree))}`,
  ).toBe(true)
  const control = countControl(tree)
  expect(control !== undefined && hasWord(textOf(control), '3 delegates'), `the band holds no control counting 3 delegates: ${clip(textOf(tree))}`).toBe(true)

  const chips = accounts.map(account => chipFor(tree, account, accounts))
  for (const [i, chip] of chips.entries()) {
    expect(chip !== undefined, `the band holds no pressable chip for ${accounts[i]}: ${clip(textOf(tree))}`).toBe(true)
  }
  const order = [control!, ...chips.map(chip => chip!)].map(el => docIndex(tree, el))
  expect(order, 'the band does not draw the count control first, then the chips newest first (D1, zcode, D4)').toEqual([...order].sort((a, b) => a - b))
  const shown = frames.find(frame => glyphLeaves(frame, [LEAD_DOT]).length === 1)!
  const lead = glyphLeaves(shown, [LEAD_DOT])[0]!
  expect(docIndex(shown, lead) < docIndex(shown, countControl(shown)!), 'the lead dot does not come before the count control').toBe(true)
  await band.unmount()
})

test('M3c-2: each chip draws its state mark, its account in clauth blue and its age dim; a running mark is Claude Code\'s orange, a finished one green', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const jobs = bandBatch(world.clock.now())
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const accounts = jobs.map(j => j.profile)
  const band = await mountBand($)
  const tree = await bandFrame(band)
  const now = world.clock.now()

  for (const job of jobs) {
    const chip = chipFor(tree, job.profile, accounts)
    expect(chip !== undefined, `the band holds no chip for ${job.profile}`).toBe(true)
    const isRunning = job.state === 'running'
    const marks = glyphLeaves(chip, isRunning ? RUNNING_MARKS : [DONE_MARK])
    expect(marks.length, `the chip for ${job.profile} draws no ${isRunning ? 'running' : 'done'} mark: ${clip(textOf(chip))}`).toBe(1)
    expect(colourOf(tree, marks[0]!), `the ${job.profile} chip's mark is not ${isRunning ? 'orange' : 'green'}`).toBe(isRunning ? 'claude' : 'success')

    const account = leafWith(chip, job.profile)
    expect(account !== undefined && colourOf(tree, account) === ACCOUNT_BLUE, `the ${job.profile} chip's account is not clauth blue`).toBe(true)

    const sinceStart = Math.floor((now - job.startedAt) / 1000)
    const ageLeaf = elements(chip).filter(el => el.type === 'Text' && durationsIn(textOf(el)).length > 0 && !hasWord(textOf(el), job.profile)).at(-1)
    expect(ageLeaf !== undefined, `the ${job.profile} chip draws no age: ${clip(textOf(chip))}`).toBe(true)
    const ran = Math.floor(((job.doneAt ?? now) - job.startedAt) / 1000)
    const fits = durationsIn(textOf(ageLeaf)).some(s => s >= ran - POLL_MS / 1000 - 60 && s <= sinceStart + 60)
    expect(fits, `the ${job.profile} chip's age is not between ${ran}s and ${sinceStart}s: ${clip(textOf(ageLeaf))}`).toBe(true)
    expect(isDim(tree, ageLeaf!), `the ${job.profile} chip's age is not dim`).toBe(true)
  }
  await band.unmount()
})

// A `#rrggbb` colour's brightness, 0 to 255; undefined for anything else.
function brightnessOf(colour: string | undefined): number | undefined {
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(colour ?? '')
  if (m === null) return undefined
  return (Number.parseInt(m[1]!, 16) + Number.parseInt(m[2]!, 16) + Number.parseInt(m[3]!, 16)) / 3
}

test('M3c-3: while any delegate runs the lead dot stays drawn and its colour fades down and back up, and the running mark cycles in orange', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzc3aaa-0', 'D1', 60_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const band = await mountBand($)

  // Claude Code's dot fades to half-dim and back over 2 s and never turns off:
  // sampled every 250 ms over two cycles.
  const fade = await bandFrames(band, world, 16, 250)
  const levels: number[] = []
  for (const [i, frame] of fade.entries()) {
    const dots = glyphLeaves(frame, [LEAD_DOT])
    expect(dots.length, `the lead dot is not drawn at ${i * 250} ms: ${clip(textOf(frame))}`).toBe(1)
    const level = brightnessOf(colourOf(frame, dots[0]!))
    expect(level !== undefined, `the lead dot's colour at ${i * 250} ms is no #rrggbb a fade can step: ${colourOf(frame, dots[0]!)}`).toBe(true)
    levels.push(level!)
  }
  const [low, high] = [Math.min(...levels), Math.max(...levels)]
  expect(low <= high * 0.75, `the lead dot never dims: brightness ${levels.join(',')}`).toBe(true)
  expect(new Set(levels).size >= 3, `the lead dot flips between two colours instead of fading: ${levels.join(',')}`).toBe(true)
  const lowAt = levels.indexOf(low)
  expect(levels.slice(lowAt).some(level => level >= low + (high - low) / 2), `the lead dot dims and never comes back: ${levels.join(',')}`).toBe(true)
  // A 2 s cycle: samples 2 s apart match (one frame's drift aside), samples
  // 1 s apart sit across the cycle from each other.
  const span = high - low
  const fullCycle = Math.max(...levels.slice(0, 8).map((level, i) => Math.abs(level - levels[i + 8]!)))
  const halfCycle = Math.max(...levels.slice(0, 12).map((level, i) => Math.abs(level - levels[i + 4]!)))
  expect(fullCycle <= span / 4, `the lead dot's fade does not repeat every 2 s: ${levels.join(',')}`).toBe(true)
  expect(halfCycle >= span / 2, `the lead dot's fade repeats in under 2 s: ${levels.join(',')}`).toBe(true)

  // Sampled four times a second, finer than any cadence a mark could cycle at.
  const quarters = await bandFrames(band, world, 16, BLINK_MS / 4)
  const seen = new Set<string>()
  for (const frame of quarters) {
    const marks = glyphLeaves(chipFor(frame, 'D1', ['D1']), RUNNING_MARKS)
    expect(marks.length, `a frame's D1 chip draws no running mark: ${clip(textOf(frame))}`).toBe(1)
    expect(colourOf(frame, marks[0]!), 'a running mark is not orange').toBe('claude')
    seen.add(textOf(marks[0]).trim())
  }
  expect(seen.size >= 3, `the running mark did not cycle over four seconds: ${[...seen].join(' ')}`).toBe(true)
  await band.unmount()
})

// Claude Code 2.1.325's spinner on a terminal that is neither ghostty nor
// macOS (measured 2026-11-14 in its bundle): the glyphs forward then back,
// one frame per 120 ms off the clock (`Math.floor(now / 120) % frames.length`).
const CC_SPINNER_FRAME_MS = 120
const CC_SPINNER_CYCLE = ['·', '✢', '*', '✶', '✻', '✽', '✽', '✻', '✶', '*', '✢', '·']

test('M3c-3b: a running chip\'s mark steps through Claude Code\'s spinner cycle, forward then back, one frame per 120 ms', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzc3bbb-0', 'D1', 60_000))
  await world.clock.advance(POLL_MS)
  const band = await mountBand($)
  // Sample mid-frame, clear of every frame boundary.
  const phase = world.clock.now() % CC_SPINNER_FRAME_MS
  await world.clock.advance((CC_SPINNER_FRAME_MS - phase) % CC_SPINNER_FRAME_MS + CC_SPINNER_FRAME_MS / 2)
  const frames = await bandFrames(band, world, 2 * CC_SPINNER_CYCLE.length, CC_SPINNER_FRAME_MS)
  const marks = frames.map(frame => textOf(glyphLeaves(chipFor(frame, 'D1', ['D1']), RUNNING_MARKS)[0]).trim())
  const fits = CC_SPINNER_CYCLE.some((_, start) =>
    marks.every((mark, i) => mark === CC_SPINNER_CYCLE[(start + i) % CC_SPINNER_CYCLE.length]))
  expect(fits, `the running mark does not step through Claude Code's cycle at 120 ms: ${marks.join(' ')}`).toBe(true)
  await band.unmount()
})

// Claude Code's 120 ms spinner picks only ghostty's glyphs or the set above
// (measured 2026-11-17 in the 2.1.325 bundle: `TERM==="xterm-ghostty"`), so a
// macOS session draws the same cycle as any other non-ghostty terminal.
test('M3c-3c: on macOS a running chip\'s mark steps through the same cycle as any non-ghostty terminal', async ($, on) => {
  on('process.run', { argv: [/^uname$/] }, () => processResult(0, 'Darwin\n'))
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzc3ccc-0', 'D1', 60_000))
  await world.clock.advance(POLL_MS)
  const band = await mountBand($)
  const phase = world.clock.now() % CC_SPINNER_FRAME_MS
  await world.clock.advance((CC_SPINNER_FRAME_MS - phase) % CC_SPINNER_FRAME_MS + CC_SPINNER_FRAME_MS / 2)
  const frames = await bandFrames(band, world, 2 * CC_SPINNER_CYCLE.length, CC_SPINNER_FRAME_MS)
  const marks = frames.map(frame => textOf(glyphLeaves(chipFor(frame, 'D1', ['D1']), RUNNING_MARKS)[0]).trim())
  const fits = CC_SPINNER_CYCLE.some((_, start) =>
    marks.every((mark, i) => mark === CC_SPINNER_CYCLE[(start + i) % CC_SPINNER_CYCLE.length]))
  expect(fits, `the running mark on macOS does not step through Claude Code's non-ghostty cycle: ${marks.join(' ')}`).toBe(true)
  await band.unmount()
})

test('M3c-4: once every delegate is done the band reads `N delegates done`, faint, with a steady dot and the count as the one control; the next delegate starts a new batch', { plugins: [PANE_CLOSER] }, async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = bandBatch(world.clock.now())
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  for (const job of jobs) world.job({ ...job, state: 'done', doneAt: job.doneAt ?? world.clock.now() })
  await world.clock.advance(POLL_MS)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const paneId = world.opens.at(-1)!.id
  let band = await mountBand($)

  const done = await bandFrames(band, world, 4, BLINK_MS)
  for (const frame of done) {
    expect(hasWord(textOf(frame), '3 delegates done'), `the all-done line does not count 3 delegates done: ${clip(textOf(frame))}`).toBe(true)
    const buttons = buttonsIn(frame)
    expect(buttons.length === 1 && hasWord(textOf(buttons[0]), '3 delegates'), `the all-done line's one control is not its 3 delegates count: ${clip(textOf(frame))}`).toBe(true)
    for (const account of ['D1', 'zcode', 'D4']) {
      expect(hasWord(textOf(frame), account), `the all-done line still names ${account}: ${clip(textOf(frame))}`).toBe(false)
    }
    const dot = glyphLeaves(frame, [LEAD_DOT])
    expect(dot.length, 'a frame of the all-done line draws no steady lead dot').toBe(1)
    const row = elements(frame).find(el => (el as { part?: { plugin?: string } }).part?.plugin === 'clauth')
    const bright = elements(row).filter(el => el.type === 'Text' && textOf(el).trim() !== '' && !isDim(frame, el))
    expect(bright.map(el => textOf(el)), 'the all-done line draws text that is not faint').toEqual([])
  }

  await $.turn.start({ text: 'close the panes', turnId: 't-close-done' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close-done' })
  expect(world.open.has(paneId), 'the pane stayed open after it was closed').toBe(false)
  await band.press({ key: keyOf(countControl(await bandFrame(band))!) })
  await world.clock.settle()
  expect(world.open.has(paneId), 'pressing the all-done line\'s count did not open the delegates pane').toBe(true)

  const next = ownJob(world.clock.now(), 'd-mvzc4ddd-3', 'D1', 1_000)
  await delegate($, world, next)
  band = await advanceBandUndrawn($, world, band)
  let tree = await bandFrame(band)
  const control = countControl(tree)
  expect(control !== undefined && hasWord(textOf(control), '1 delegate'), `the new batch's control does not count 1 delegate: ${clip(textOf(tree))}`).toBe(true)
  expect(chipFor(tree, 'D1', ['D1', 'zcode', 'D4']) !== undefined, 'the new batch draws no chip for its D1').toBe(true)
  for (const account of ['zcode', 'D4']) {
    expect(hasWord(textOf(tree), account), `the new batch still shows ${account} from the finished one: ${clip(textOf(tree))}`).toBe(false)
  }

  world.job({ ...next, state: 'done', doneAt: world.clock.now() })
  band = await advanceBandUndrawn($, world, band)
  tree = await bandFrame(band)
  expect(hasWord(textOf(tree), '4 delegates done'), `the all-done line does not count the session's 4 delegates: ${clip(textOf(tree))}`).toBe(true)
  await band.unmount()
})

test('M3c-5: an orphaned delegate\'s chip draws a red ✗ and a stopped one a dim ■', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const jobs = [
    ownJob(t, 'd-mvzc5aaa-0', 'D1', 60_000),
    ownJob(t, 'd-mvzc5bbb-1', 'D2', 120_000, { state: 'done', doneAt: t - 1_000, crashed: true }),
    ownJob(t, 'd-mvzc5ccc-2', 'D4', 180_000, { state: 'done', doneAt: t - 1_000, stopped: true }),
  ]
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const accounts = jobs.map(j => j.profile)
  const band = await mountBand($)
  const tree = await bandFrame(band)

  const failed = glyphLeaves(chipFor(tree, 'D2', accounts), [FAILED_MARK])
  expect(failed.length, `the orphaned D2 chip draws no ${FAILED_MARK}: ${clip(textOf(tree))}`).toBe(1)
  expect(colourOf(tree, failed[0]!), `the orphaned D2 chip's ${FAILED_MARK} is not red`).toBe('error')
  const stopped = glyphLeaves(chipFor(tree, 'D4', accounts), [STOPPED_MARK])
  expect(stopped.length, `the stopped D4 chip draws no ${STOPPED_MARK}: ${clip(textOf(tree))}`).toBe(1)
  expect(isDim(tree, stopped[0]!), `the stopped D4 chip's ${STOPPED_MARK} is not dim`).toBe(true)
  await band.unmount()
})

test('M3c-6: a batch wider than the band draws the chips that fit, then `+N` for the rest', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const accounts = ['D0', 'D1', 'D2', 'D3', 'D4', 'D5']
  for (const [i, account] of accounts.entries()) {
    await delegate($, world, ownJob(t, `d-mvzc6aa${i}-${i}`, account, 60_000 * (i + 1)))
  }
  await world.clock.advance(POLL_MS)
  const band = await mountBand($, 40)
  const tree = await bandFrame(band)
  const control = countControl(tree)
  expect(control !== undefined && hasWord(textOf(control), '6 delegates'), `the narrow band lost its count control: ${clip(textOf(tree))}`).toBe(true)
  const shown = accounts.filter(account => chipFor(tree, account, accounts) !== undefined).length
  const more = /\+(\d+)\b/.exec(textOf(tree))
  expect(shown >= 1 && shown < accounts.length, `the 40-column band shows ${shown} of 6 chips: ${clip(textOf(tree))}`).toBe(true)
  expect(more !== null && shown + Number(more[1]) === accounts.length, `the band's +N does not count the ${accounts.length - shown} hidden chips: ${clip(textOf(tree))}`).toBe(true)
  await band.unmount()
})

test('M3c-7: with delegates in the session the hint line under the prompt is the engine\'s own, with no control added', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzc7aaa-0', 'D1', 1_000))
  await world.clock.advance(POLL_MS)
  const hint = await mountHint($)
  await hint.redraw()
  const tree = await hint.drawn()
  expect(textOf(tree).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(textOf(tree))}`).toBe(true)
  expect(buttonsIn(tree).length, `the hint line still holds a delegates control: ${clip(textOf(tree))}`).toBe(0)
  await hint.unmount()
})

// The model a delegate's transcript records on each assistant message, as a
// DeepSeek-backed run writes it (measured 2026-11-14: `"model":"deepseek-v4-pro"`).
const DELEGATE_MODEL = 'deepseek-v4-flash'
const SKILL_TEXT = 'Base directory for this skill'

// A delegate transcript as Claude Code writes it: the delegate's own prompt
// (two lines), a skill load's injected text (`isMeta`), then tool calls and
// the delegate's last message. `filler` puts 600 KB of calls between the
// prompt and the rest, past the 512 KiB the pane reads from the end.
function delegateTranscript(tag: string, sessionId: string, cwd: string, filler = false): string {
  const base = { sessionId, cwd, version: '2.1.325', isSidechain: false }
  const lines: string[] = []
  let n = 0
  const row = (o: Record<string, unknown>) => {
    n += 1
    lines.push(JSON.stringify({ ...o, ...base, uuid: `${tag}-u${n}`, timestamp: new Date(T0 - 900_000 + n * 10).toISOString() }))
  }
  const say = (content: unknown[]) => row({ type: 'assistant', message: { role: 'assistant', model: DELEGATE_MODEL, content } })
  row({ type: 'user', message: { role: 'user', content: `${tag}: the fix18 review findings\nsecond line of ${tag}` } })
  row({
    type: 'user',
    isMeta: true,
    sourceToolUseID: `call_${tag}_skill`,
    message: { role: 'user', content: [{ type: 'text', text: `${SKILL_TEXT}: /home/u/.claude/skills/example-skill\n\n# Example skill` }] },
  })
  if (filler) {
    const pad = 'x'.repeat(200)
    for (let i = 0; i < 1600; i += 1) {
      say([{ type: 'tool_use', id: `toolu_${tag}f${i}`, name: 'Bash', input: { command: `echo ${i} ${pad}` } }])
      row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}f${i}`, content: `${i}` }] } })
    }
  }
  say([{ type: 'tool_use', id: `toolu_${tag}b`, name: 'Bash', input: { command: 'cargo test' } }])
  row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}b`, content: `${tag} 12 passed\nsecond result line` }] } })
  say([{ type: 'tool_use', id: `toolu_${tag}r`, name: 'Read', input: { file_path: `${HOME}/work/${tag}/code.md` } }])
  row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}r`, content: 'the file body' }] } })
  // A Read under the delegate's own working directory, recorded as Claude Code
  // records one (measured 2026-11-14): `<n>\t<line>` content beside
  // `toolUseResult.file.numLines`.
  const libPath = `${cwd}/src/lib.ts`
  say([{ type: 'tool_use', id: `toolu_${tag}l`, name: 'Read', input: { file_path: libPath } }])
  row({
    type: 'user',
    message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}l`, content: '1\tone\n2\ttwo\n3\tthree\n4\tfour\n5\tfive' }] },
    toolUseResult: { type: 'text', file: { filePath: libPath, content: 'one\ntwo\nthree\nfour\nfive', numLines: 5, startLine: 1, totalLines: 5 } },
  })
  say([{ type: 'text', text: `**Step two:** ${tag} read the build config and found three things worth fixing before the next round, then wrote each one down as ${tag}-long-tail` }])
  say([{ type: 'tool_use', id: `toolu_${tag}o`, name: 'Bash', input: { command: 'ls src' } }])
  row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `toolu_${tag}o`, content: `${tag}-out-1\n${tag}-out-2\n${tag}-out-3\n${tag}-out-4\n${tag}-out-5\n${tag}-out-6` }] } })
  say([{ type: 'text', text: `${tag}-reply-final` }])
  row({ type: 'system', subtype: 'turn_duration', durationMs: 1200 })
  return `${lines.join('\n')}\n`
}

// Three delegates of this session, each with its own transcript on disk.
function paneBatch(world: World): Job[] {
  const t = world.clock.now()
  const jobs = [
    ownJob(t, 'd-mvzc8aaa-0', 'D1', 300_000, { sessionId: 'sess-c8-alpha', cwd: '/work/alpha' }),
    ownJob(t, 'd-mvzc8bbb-1', 'zcode', 600_000, { sessionId: 'sess-c8-beta', cwd: '/work/beta', state: 'done', doneAt: t - 60_000 }),
    ownJob(t, 'd-mvzc8ccc-2', 'D4', 900_000, { sessionId: 'sess-c8-gamma', cwd: '/work/gamma' }),
  ]
  const tags = ['alpha', 'beta', 'gamma']
  for (const [i, job] of jobs.entries()) {
    world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript(tags[i]!, job.sessionId!, job.cwd!))
  }
  return jobs
}

// The pane mounted with room for the whole fixture transcript: the pane draws
// only the newest items that fit its rows.
async function mountTallPane($: Engine, world: World): Promise<Pane> {
  await askForPane($, world)
  const opened = world.opens.at(-1)
  expect(opened !== undefined, 'the mod opened no pane').toBe(true)
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'Pane',
    requestId: opened!.id,
    props: { ...PANE_PROPS, title: opened!.title ?? opened!.id, scroll: { offset: 0, bodyRows: 200 } },
  })
}

async function mountCoveredPane($: Engine, world: World, isPromptCovered: boolean): Promise<Pane> {
  await askForPane($, world)
  const opened = world.opens.at(-1)
  expect(opened !== undefined, 'the mod opened no pane').toBe(true)
  return $.ui.mount({
    plugin: 'clauth',
    surface: 'terminal',
    component: 'Pane',
    requestId: opened!.id,
    props: { ...PANE_PROPS, title: opened!.title ?? opened!.id, ...(isPromptCovered ? { isPromptCovered: true as const } : {}) },
  })
}

test('M3c-8: pressing a chip opens the pane on that delegate, and the count control opens it too', { plugins: [PANE_CLOSER] }, async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const paneId = world.opens.at(-1)!.id
  const accounts = jobs.map(j => j.profile)
  let band = await mountBand($)

  for (const [job, tag, other] of [[jobs[2]!, 'gamma', 'alpha'], [jobs[0]!, 'alpha', 'gamma']] as const) {
    await $.turn.start({ text: 'close the panes', turnId: `t-close-${tag}` })
    await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: `t-close-${tag}` })
    expect(world.open.has(paneId), 'the pane stayed open after it was closed').toBe(false)
    const chip = chipFor(await bandFrame(band), job.profile, accounts)
    expect(chip !== undefined, `the band holds no chip for ${job.profile}`).toBe(true)
    await band.press({ key: keyOf(chip!) })
    band = await advanceBandUndrawn($, world, band)
    expect(world.open.has(paneId), `pressing the ${job.profile} chip did not open the delegates pane`).toBe(true)
    const pane = await mountPane($, world)
    const text = textOf(await drawn(pane))
    expect(hasWord(text, job.id), `the pane opened from the ${job.profile} chip does not show ${job.id}: ${clip(text)}`).toBe(true)
    expect(hasWord(text, `${tag}-reply-final`), `the pane opened from the ${job.profile} chip does not show its transcript: ${clip(text)}`).toBe(true)
    expect(hasWord(text, `${other}-reply-final`), `the pane opened from the ${job.profile} chip shows another delegate's transcript`).toBe(false)
    await pane.unmount()
  }

  await $.turn.start({ text: 'close the panes', turnId: 't-close-count' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close-count' })
  await band.press({ key: keyOf(countControl(await bandFrame(band))!) })
  await world.clock.settle()
  expect(world.open.has(paneId), 'pressing the count control did not open the delegates pane').toBe(true)
  await band.unmount()
})

test('M3c-8b: pressing a finished delegate\'s chip opens the pane on that delegate', async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const done = jobs[1]!
  let band = await mountBand($)
  const chip = chipFor(await bandFrame(band), done.profile, jobs.map(j => j.profile))
  expect(chip !== undefined, `the band holds no chip for the finished ${done.profile}`).toBe(true)
  await band.press({ key: keyOf(chip!) })
  band = await advanceBandUndrawn($, world, band)
  const pane = await mountPane($, world)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'beta-reply-final'), `the pane opened from the finished ${done.profile} chip does not show its transcript: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), 'the pane opened from a finished chip shows the newest delegate instead').toBe(false)
  await pane.unmount()
  await band.unmount()
})

// Each holds one `tail` read of one transcript for 5 s on the mocked clock,
// long enough for the store's ticks to run while a choice reads it, and toasts
// `held <session>` so a test can count it. The kit loads a test plugin's
// `register` on its own, so each spells its session inline. The first holds
// that transcript's first read; the second arms once a pane closes, holding
// the read a reopen makes.
const HOLD_FIRST_GAMMA_READ: Plugin = {
  name: 'hold-first-gamma-read',
  register(on) {
    let armed = true
    on('process.run', async ($, e, next) => {
      if (armed && e.argv[0] === 'tail' && String(e.argv[e.argv.length - 1] ?? '').includes('sess-c8-gamma')) {
        armed = false
        $.ui.toast('held sess-c8-gamma')
        await $.clock.sleep(5_000)
      }
      return next(e)
    })
  },
}

const HOLD_REOPEN_ALPHA_READ: Plugin = {
  name: 'hold-reopen-alpha-read',
  register(on) {
    let armed = false
    on('ui.close', async ($, e, next) => {
      armed = true
      return next(e)
    })
    on('process.run', async ($, e, next) => {
      if (armed && e.argv[0] === 'tail' && String(e.argv[e.argv.length - 1] ?? '').includes('sess-c14c-alpha')) {
        armed = false
        $.ui.toast('held sess-c14c-alpha')
        await $.clock.sleep(5_000)
      }
      return next(e)
    })
  },
}

const heldReads = (world: World, session: string): number => world.notices.filter(n => n.text === `held ${session}`).length

test('M3c-8c: a chip pressed while delegates run opens the pane on its delegate even when a store tick lands during the choice', { plugins: [HOLD_FIRST_GAMMA_READ] }, async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let band = await mountBand($)
  const chip = chipFor(await bandFrame(band), 'D4', jobs.map(j => j.profile))
  expect(chip !== undefined, 'the band holds no chip for D4').toBe(true)
  await band.press({ key: keyOf(chip!) })
  await world.clock.settle()
  expect(heldReads(world, 'sess-c8-gamma'), 'the D4 transcript read was not held').toBe(1)
  band = await advanceBandUndrawn($, world, band)
  const pane = await mountPane($, world)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'gamma-reply-final'), `the pane opened from the D4 chip does not show D4's transcript: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), 'a store tick during the choice put the pane on the newest delegate').toBe(false)
  await pane.unmount()
  await band.unmount()
})

test('M3c-9: the pane lists every delegate beside the selected one\'s detail, and choosing another in the list swaps only the detail', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const accounts = jobs.map(j => j.profile)
  let pane = await mountPane($, world)

  let tree = await drawn(pane)
  for (const account of accounts) {
    expect(hasWord(textOf(tree), account), `the pane's list leaves out ${account}: ${clip(textOf(tree))}`).toBe(true)
  }
  expect(hasWord(textOf(tree), 'alpha-reply-final'), `the pane does not open on the newest delegate, D1: ${clip(textOf(tree))}`).toBe(true)

  const entry = chipFor(tree, 'D4', accounts)
  expect(entry !== undefined, `the pane's list holds no pressable entry for D4: ${clip(textOf(tree))}`).toBe(true)
  await pane.press({ key: keyOf(entry!) })
  pane = await advanceUndrawn($, world, pane)
  tree = await drawn(pane)
  expect(hasWord(textOf(tree), 'gamma-reply-final'), `choosing D4 does not show its transcript: ${clip(textOf(tree))}`).toBe(true)
  expect(hasWord(textOf(tree), 'alpha-reply-final'), 'choosing D4 still shows D1\'s transcript').toBe(false)
  for (const account of accounts) {
    expect(hasWord(textOf(tree), account), `choosing D4 dropped ${account} from the list: ${clip(textOf(tree))}`).toBe(true)
  }
  await pane.unmount()
})

test('M3c-10: the detail header names the delegate by its prompt\'s first line, then its model and job id dim', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzcaaaa-0', 'D1', 300_000, { sessionId: 'sess-c10', cwd: '/work/alpha' })
  const transcript = delegateTranscript('alpha', job.sessionId!, job.cwd!, true)
  expect(new TextEncoder().encode(transcript).length > 512 * 1024, 'the fixture does not pass the 512 KiB the pane reads from the end').toBe(true)
  world.put(transcriptPath(job.cwd!, job.sessionId!), transcript)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  const text = textOf(tree)

  const title = 'alpha: the fix18 review findings'
  expect(text.includes(title), `the header does not name the delegate by its prompt's first line: ${clip(text)}`).toBe(true)
  expect(text.split(title).length - 1, `the prompt's first line is drawn again below the header: ${clip(text)}`).toBe(1)
  const titleLeaf = elements(tree).filter(el => el.type === 'Text' && textOf(el).includes(title)).at(-1)
  expect(
    String(titleLeaf?.props?.wrap ?? '').startsWith('truncate'),
    `the title may wrap onto a second line: ${clip(JSON.stringify(titleLeaf?.props ?? {}))}`,
  ).toBe(true)
  // The title's own row holds the running delegate's stop action, and nothing
  // of the facts line. The pane draws no close action: Claude Code's frame
  // around a pane already carries one.
  const titleRow = elements(tree)
    .filter(el => elements(el).includes(titleLeaf!) && buttonsIn(el).some(b => /\bstop\b/i.test(textOf(b))))
    .at(-1)
  expect(
    titleRow !== undefined && !hasWord(textOf(titleRow), DELEGATE_MODEL),
    `the stop action is not on the title's own line: ${clip(text)}`,
  ).toBe(true)
  const closeActions = buttonsIn(tree).filter(b => /\bclose\b/i.test(textOf(b)))
  expect(closeActions.length, `the pane draws its own close action: ${clip(text)}`).toBe(0)
  expect(text.includes(SKILL_TEXT), `the pane shows a skill's injected text: ${clip(text)}`).toBe(false)
  for (const word of [DELEGATE_MODEL, job.id]) {
    const leaf = leafWith(tree, word)
    expect(leaf !== undefined, `the header does not show ${word}: ${clip(text)}`).toBe(true)
    expect(isDim(tree, leaf!), `the header's ${word} is not dim`).toBe(true)
  }
  const facts = elements(tree)
    .filter(el => el.type === 'Text' && new RegExp(`\\bD1\\b.*${escapeRe(DELEGATE_MODEL)}.*${escapeRe(job.id)}`).test(textOf(el)))
    .at(-1)
  expect(facts !== undefined, `no header line reads D1 · ${DELEGATE_MODEL} · ${job.id}: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

// The status line reads `<state> · N tool calls · last output Xs ago` for the
// selected delegate, so a stop at exit, a cancel and a dead owner stay told
// apart where their marks cannot. The fixture transcript holds 4 calls.
const FIXTURE_TOOL_CALLS = 4
for (const [state, extra] of [
  ['running', {}],
  ['done', { state: 'done', doneAt: T0 }],
  ['stopped', { state: 'done', doneAt: T0, stopped: true }],
  ['cancelled', { state: 'done', doneAt: T0, cancelled: true }],
  ['orphaned', { state: 'done', doneAt: T0, crashed: true }],
] as const) {
  test(`M3c-13: the status line reads the selected delegate's state and tool-call count (${state})`, async ($, on) => {
    const world = installWorld(on)
    await startSession($)
    const job = ownJob(world.clock.now(), 'd-mvzcdaaa-0', 'D1', 300_000, { sessionId: 'sess-c13', cwd: '/work/alpha', ...extra })
    world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript('alpha', job.sessionId!, job.cwd!))
    await delegate($, world, job)
    await world.clock.advance(POLL_MS)
    const pane = await mountPane($, world)
    const tree = await drawn(pane)
    const line = elements(tree)
      .filter(el => el.type === 'Text' && new RegExp(`\\b${state}\\b.*\\b${FIXTURE_TOOL_CALLS} tool calls\\b`).test(textOf(el)))
      .at(-1)
    expect(line !== undefined, `no status line reads ${state} · ${FIXTURE_TOOL_CALLS} tool calls: ${clip(textOf(tree))}`).toBe(true)
    const others = ['running', 'done', 'stopped', 'cancelled', 'orphaned'].filter(word => word !== state)
    for (const word of others) {
      expect(hasWord(textOf(line), word), `the ${state} delegate's status line also says ${word}: ${clip(textOf(line))}`).toBe(false)
    }
    if (state === 'running') {
      expect(/\blast output\b/.test(textOf(line)) && durationsIn(textOf(line)).length > 0, `the running status line names no last output age: ${clip(textOf(line))}`).toBe(true)
    }
    await pane.unmount()
  })
}

// The status line's Text: the innermost one naming `state` and a tool-call count.
function statusLine(root: unknown, state: string): Drawn | undefined {
  return elements(root)
    .filter(el => el.type === 'Text' && new RegExp(`\\b${state}\\b.*\\b\\d+ tool calls?\\b`).test(textOf(el)))
    .at(-1)
}

test('M3c-16: the status line sits below the transcript\'s newest item, with nothing drawn after it, and the header keeps the title and the facts line above the transcript', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzcgaaa-0', 'D1', 300_000, { sessionId: 'sess-c16', cwd: '/work/alpha' })
  world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript('alpha', job.sessionId!, job.cwd!))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  const tree = await drawn(pane)
  const text = textOf(tree)
  const all = elements(tree)
  expect(text.includes(SKILL_TEXT), `the pane shows a skill's injected text: ${clip(text)}`).toBe(false)

  const status = statusLine(tree, 'running')
  expect(status !== undefined, `no status line reads running · N tool calls: ${clip(text)}`).toBe(true)
  const newest = leafWith(tree, 'alpha-reply-final')
  expect(newest !== undefined, `the transcript's newest item is not drawn: ${clip(text)}`).toBe(true)
  const title = elements(tree).filter(el => el.type === 'Text' && textOf(el).includes('alpha: the fix18 review findings')).at(-1)
  const facts = elements(tree)
    .filter(el => el.type === 'Text' && new RegExp(`\\bD1\\b.*${escapeRe(job.id)}`).test(textOf(el)))
    .at(-1)
  expect(title !== undefined && facts !== undefined, `the header lost its title or facts line: ${clip(text)}`).toBe(true)
  const firstItem = leafWith(tree, 'alpha 12 passed') ?? newest!
  expect(all.indexOf(title!) < all.indexOf(firstItem) && all.indexOf(facts!) < all.indexOf(firstItem), 'the title or the facts line is drawn below the transcript').toBe(true)
  expect(all.indexOf(status!) > all.indexOf(newest!), `the status line is drawn above the transcript's newest item: ${clip(text)}`).toBe(true)
  const after = all
    .slice(all.indexOf(status!) + 1)
    .filter(el => !elements(status).includes(el) && el.type === 'Text' && textOf(el).trim() !== '')
  expect(after.map(el => textOf(el)), 'something is drawn below the status line').toEqual([])
  await pane.unmount()
})

// One `ui.scroll` over the delegates pane as the engine raises it for the
// person: a wheel over the transcript carries the pointer's cell, a page key
// none. The mod draws a tree that fits the body, so the engine's own window
// sits at 0 and the move is the mod's to make.
async function scrollPane($: Engine, paneId: string, by: number, wheel: boolean): Promise<void> {
  await $.ui.scroll({
    component: 'Pane',
    requestId: paneId,
    offset: 0,
    by,
    bodyRows: PANE_PROPS.scroll.bodyRows,
    contentRows: PANE_PROPS.scroll.bodyRows,
    origin: { kind: 'person' },
    ...(wheel ? { pointer: { column: PANE_PROPS.bodyColumns - 10, row: Math.floor(PANE_PROPS.scroll.bodyRows / 2) } } : {}),
  })
}

// The `<tag>-filler-<n>` message numbers a drawing shows.
function fillersIn(text: string, tag: string): number[] {
  return [...text.matchAll(new RegExp(`(?<![A-Za-z0-9_-])${escapeRe(tag)}-filler-(\\d+)(?![A-Za-z0-9_-])`, 'g'))].map(m => Number(m[1]))
}

test('M3c-17: the transcript opens at the end: the newest item drawn and the window showing only the newest rounds', async ($, on) => {
  const { pane } = await scrolledPane($, on, 'd-mvzchaaa-0', 'sess-c17')
  const tree = await drawn(pane)
  const text = textOf(tree)
  expect(hasWord(text, 'alpha-reply-final'), `the pane does not open on the newest delegate's end: ${clip(text)}`).toBe(true)
  const atEnd = fillersIn(text, 'alpha')
  expect(atEnd.length > 0 && Math.min(...atEnd) >= 150, `the window at the end shows older items than the newest: ${atEnd.join(',')}`).toBe(true)
  await pane.unmount()
})

test('M3c-17h: a wheel up over the transcript brings older items into view while the header, the list and the status line stay', async ($, on) => {
  // The engine beneath: its window over a tree that fits stays where it is.
  on('ui.scroll', () => ({}))
  const world = installWorld(on)
  await startSession($)
  const cwd = '/work/alpha'
  const t = world.clock.now()
  const job = ownJob(t, 'd-mvzchaaa-0', 'D1', 300_000, { sessionId: 'sess-c17h', cwd })
  const older = ownJob(t, 'd-mvzchbbb-1', 'D4', 600_000, { sessionId: 'sess-c17h-gamma', cwd: '/work/gamma' })
  world.put(transcriptPath(cwd, job.sessionId!), transcriptFile('alpha', job.sessionId!, cwd))
  world.put(transcriptPath(older.cwd!, older.sessionId!), delegateTranscript('gamma', older.sessionId!, older.cwd!))
  await delegate($, world, older)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  const paneId = world.opens.at(-1)!.id

  let tree = await drawn(pane)
  let text = textOf(tree)
  expect(hasWord(text, 'alpha-reply-final'), `the pane does not open on the newest delegate's end: ${clip(text)}`).toBe(true)
  const atEnd = fillersIn(text, 'alpha')
  expect(atEnd.length > 0 && Math.min(...atEnd) >= 150, `the window at the end shows older items than the newest: ${atEnd.join(',')}`).toBe(true)

  // A wheel up over the transcript: older items come into view, the newest leaves.
  await scrollPane($, paneId, -60, true)
  await world.clock.settle()
  tree = await drawn(pane)
  text = textOf(tree)
  const scrolled = fillersIn(text, 'alpha')
  expect(hasWord(text, 'alpha-reply-final'), `a wheel up left the newest item in view: ${clip(text)}`).toBe(false)
  expect(scrolled.length > 0 && Math.min(...scrolled) < Math.min(...atEnd), `a wheel up brought no older item into view: ${scrolled.join(',')} after ${atEnd.join(',')}`).toBe(true)
  expect(text.includes('alpha-user-first'), `the title scrolled away with the transcript: ${clip(text)}`).toBe(true)
  expect(new RegExp(`\\bD1\\b.*${escapeRe(job.id)}`).test(text), `the facts line scrolled away: ${clip(text)}`).toBe(true)
  expect(statusLine(tree, 'running') !== undefined, `the status line scrolled away: ${clip(text)}`).toBe(true)
  expect(entryFor(tree, older.id) !== undefined, `the list lost the other delegate's entry: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

// One assistant text line of a delegate's transcript, as Claude Code writes it.
const saidLine = (text: string, sessionId: string, cwd: string): string =>
  `${JSON.stringify({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text }] }, sessionId, cwd })}\n`

// The clock through one window with no drawing mounted. A mounted pane or band
// showing a live delegate asks for a frame every 120 ms, and the kit's
// `advance` resolves every wait due on its way, so a whole window under either
// costs a render per frame. The pane stays open to the mod, whose ticks read
// the changed transcript, and the window it keeps is its own state, so the
// mount after the window draws what a stay would have drawn.
async function advanceUndrawn(
  $: Engine,
  world: World,
  pane: Pane,
  opts: { put?: () => void; ms?: number; mount?: ($: Engine, world: World) => Promise<Pane> } = {},
): Promise<Pane> {
  await pane.unmount()
  opts.put?.()
  await world.clock.advance(opts.ms ?? POLL_MS)
  return (opts.mount ?? mountPane)($, world)
}

// The same window with the band undrawn: its mark frames stop with it, and the
// mount that follows reads the store as the band's next draw would.
async function advanceBandUndrawn($: Engine, world: World, band: Band): Promise<Band> {
  await band.unmount()
  await world.clock.advance(POLL_MS)
  return mountBand($)
}

test('M3c-17i: new output while the window is scrolled up holds the window still', async ($, on) => {
  const sessionId = 'sess-c17i'
  const cwd = '/work/alpha'
  const { world, pane: opened, paneId, file, base } = await scrolledPane($, on, 'd-mvzchiii-0', sessionId)
  let pane = opened
  let tree = await drawn(pane)
  let text = textOf(tree)
  expect(hasWord(text, 'alpha-reply-final'), `the pane does not open on the newest delegate's end: ${clip(text)}`).toBe(true)
  const atEnd = fillersIn(text, 'alpha')
  expect(atEnd.length > 0 && Math.min(...atEnd) >= 150, `the window at the end shows older items than the newest: ${atEnd.join(',')}`).toBe(true)

  await scrollPane($, paneId, -60, true)
  await world.clock.settle()
  tree = await drawn(pane)
  text = textOf(tree)
  const scrolled = fillersIn(text, 'alpha')
  expect(hasWord(text, 'alpha-reply-final'), `a wheel up left the newest item in view: ${clip(text)}`).toBe(false)
  expect(scrolled.length > 0 && Math.min(...scrolled) < Math.min(...atEnd), `a wheel up brought no older item into view: ${scrolled.join(',')} after ${atEnd.join(',')}`).toBe(true)

  // New output while scrolled up: the window holds still.
  pane = await advanceUndrawn($, world, pane, { put: () => world.put(file, `${base}${saidLine('alpha-new-one', sessionId, cwd)}`) })
  tree = await drawn(pane)
  text = textOf(tree)
  expect(hasWord(text, 'alpha-new-one'), 'new output pulled a scrolled-up window to the end').toBe(false)
  expect(fillersIn(text, 'alpha'), 'new output moved a scrolled-up window').toEqual(scrolled)

  // Back at the end, the item that landed while scrolled up shows.
  await scrollPane($, paneId, 100_000, true)
  await world.clock.settle()
  text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-new-one'), `scrolling to the end does not show the item that landed while scrolled up: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3c-17j: back at the end the newest item shows, and new output follows again', async ($, on) => {
  const sessionId = 'sess-c17j'
  const cwd = '/work/alpha'
  const newOne = saidLine('alpha-new-one', sessionId, cwd)
  const newTwo = saidLine('alpha-new-two', sessionId, cwd)
  const { world, pane: opened, paneId, file, base } = await scrolledPane($, on, 'd-mvzchjjj-0', sessionId, newOne)
  let pane = opened
  let tree = await drawn(pane)
  let text = textOf(tree)
  expect(hasWord(text, 'alpha-reply-final'), `the pane does not open on the newest delegate's end: ${clip(text)}`).toBe(true)
  const atEnd = fillersIn(text, 'alpha')
  expect(atEnd.length > 0 && Math.min(...atEnd) >= 150, `the window at the end shows older items than the newest: ${atEnd.join(',')}`).toBe(true)
  await scrollPane($, paneId, -60, true)
  await world.clock.settle()
  tree = await drawn(pane)
  text = textOf(tree)
  expect(hasWord(text, 'alpha-reply-final'), `a wheel up left the newest item in view: ${clip(text)}`).toBe(false)

  // Back down to the end: the newest item shows, and new output follows.
  await scrollPane($, paneId, 100_000, true)
  await world.clock.settle()
  text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-new-one'), `scrolling to the end does not show the newest item: ${clip(text)}`).toBe(true)
  pane = await advanceUndrawn($, world, pane, { put: () => world.put(file, `${base}${newOne}${newTwo}`) })
  text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-new-two'), `a window at the end does not follow new output: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3c-17k: a page key scrolls the transcript, leaving the newest item out of view', async ($, on) => {
  const sessionId = 'sess-c17k'
  const cwd = '/work/alpha'
  const tail = `${saidLine('alpha-new-one', sessionId, cwd)}${saidLine('alpha-new-two', sessionId, cwd)}`
  const { world, pane, paneId } = await scrolledPane($, on, 'd-mvzchkkk-0', sessionId, tail)
  let text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-reply-final'), `the pane does not open on the newest delegate's end: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-new-two'), `the window at the end does not show the newest output: ${clip(text)}`).toBe(true)

  // A page key (no pointer) scrolls the transcript too.
  await scrollPane($, paneId, -PANE_PROPS.scroll.bodyRows, false)
  await world.clock.settle()
  text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-new-two'), `a page up left the newest item in view: ${clip(text)}`).toBe(false)
  await pane.unmount()
})

// A delegate of this session whose transcript is `transcriptFile`'s 200
// rounds, the delegates pane mounted on it, and its pane id.
async function scrolledPane($: Engine, on: On, id: string, sessionId: string, tail = '') {
  on('ui.scroll', () => ({}))
  const world = installWorld(on)
  await startSession($)
  const cwd = '/work/alpha'
  const job = ownJob(world.clock.now(), id, 'D1', 300_000, { sessionId, cwd })
  const file = transcriptPath(cwd, sessionId)
  const base = transcriptFile('alpha', sessionId, cwd)
  world.put(file, `${base}${tail}`)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  return { world, pane, paneId: world.opens.at(-1)!.id, file, base, cwd }
}

test('M3c-17b: one-row wheel ticks move the transcript as far as one move of the same rows, up and down', async ($, on) => {
  const { world, pane, paneId } = await scrolledPane($, on, 'd-mvzchccc-0', 'sess-c17b')
  const view = async () => fillersIn(textOf(await drawn(pane)), 'alpha')
  const ticks = async (by: number, n: number) => {
    for (let i = 0; i < n; i += 1) await scrollPane($, paneId, by, true)
    await world.clock.settle()
  }
  await ticks(-1, 60)
  const sixtyTicksUp = await view()
  expect(sixtyTicksUp.length > 0, `sixty one-row ticks up drew no transcript item: ${sixtyTicksUp.join(',')}`).toBe(true)
  await ticks(100_000, 1)
  await ticks(-60, 1)
  const oneMoveUp = await view()
  expect(sixtyTicksUp, 'sixty one-row ticks up did not move the transcript as far as one 60-row move').toEqual(oneMoveUp)
  await ticks(1, 30)
  const thirtyTicksDown = await view()
  await ticks(100_000, 1)
  await ticks(-60, 1)
  await ticks(30, 1)
  const oneMoveDown = await view()
  expect(thirtyTicksDown, 'thirty one-row ticks down did not move the transcript as far as one 30-row move').toEqual(oneMoveDown)
  await pane.unmount()
})

test('M3c-17e: one wheel tick from the end moves one row from the end: down stays at the end, up keeps the newest rounds in view', async ($, on) => {
  const { world, pane, paneId } = await scrolledPane($, on, 'd-mvzchfff-0', 'sess-c17e')
  const endFillers = fillersIn(textOf(await drawn(pane)), 'alpha')
  await scrollPane($, paneId, 1, true)
  await world.clock.settle()
  const down = textOf(await drawn(pane))
  expect(hasWord(down, 'alpha-reply-final'), `a tick down at the end left the newest item: ${clip(down)}`).toBe(true)
  expect(fillersIn(down, 'alpha'), 'a tick down at the end moved the window').toEqual(endFillers)
  await scrollPane($, paneId, -1, true)
  await world.clock.settle()
  const up = fillersIn(textOf(await drawn(pane)), 'alpha')
  expect(up.length > 0 && Math.min(...up) >= Math.min(...endFillers) - 1, `one tick up from the end jumped far back: ${up.join(',')} after ${endFillers.join(',')}`).toBe(true)
  await pane.unmount()
})

test('M3c-17f: a result landing on the call at the window\'s top holds the window on that call', async ($, on) => {
  const sessionId = 'sess-c17f'
  const cwd = '/work/alpha'
  let n = 0
  const row = (o: Record<string, unknown>) => {
    n += 1
    return `${JSON.stringify({ ...o, sessionId, cwd, uuid: `c17f-u${n}` })}\n`
  }
  const call = row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id: 'toolu_c17f_a', name: 'Bash', input: { command: 'echo alpha-inflight-a' } }] } })
  const after = Array.from({ length: 20 }, (_, i) =>
    row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text: `alpha-after-${i}` }] } })).join('')
  const result = row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_c17f_a', content: 'alpha-result-a' }] } })
  const { world, pane: opened, paneId, file, base } = await scrolledPane($, on, 'd-mvzchggg-0', sessionId, `${call}${after}`)
  let pane = opened
  // One-row ticks up until the in-flight call enters the window at its top.
  let text = textOf(await drawn(pane))
  for (let i = 0; i < 200 && !text.includes('alpha-inflight-a'); i += 1) {
    await scrollPane($, paneId, -1, true)
    await world.clock.settle()
    text = textOf(await drawn(pane))
  }
  expect(text.includes('alpha-inflight-a'), 'two hundred one-row ticks up never reached the in-flight call').toBe(true)
  expect(hasWord(text, 'alpha-after-0'), `the message after the call is not in the window: ${clip(text)}`).toBe(true)

  pane = await advanceUndrawn($, world, pane, { put: () => world.put(file, `${base}${call}${after}${result}`) })
  text = textOf(await drawn(pane))
  expect(text.includes('alpha-inflight-a'), `a result landing on the window's top call moved the window off it: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-after-0'), `a result landing on the window's top call moved the window: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3c-17c: the transcript scrolls back to the oldest item the pane read, past any fixed item count', async ($, on) => {
  const { world, pane, paneId } = await scrolledPane($, on, 'd-mvzchddd-0', 'sess-c17c')
  await scrollPane($, paneId, -100_000, true)
  await world.clock.settle()
  const text = textOf(await drawn(pane))
  // The fixture's 200 rounds are about 600 items, all inside the 512 KiB read.
  expect(hasWord(text, 'alpha-filler-0'), `the scroll stops short of the transcript's oldest message: ${fillersIn(text, 'alpha').join(',')}`).toBe(true)
  await pane.unmount()
})

test('M3c-17d: a result landing on a call while the window is scrolled up holds the window', async ($, on) => {
  const sessionId = 'sess-c17d'
  const cwd = '/work/alpha'
  const row = (o: Record<string, unknown>) => `${JSON.stringify({ ...o, sessionId, cwd })}\n`
  const call = (id: string, command: string) =>
    row({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id, name: 'Bash', input: { command } }] } })
  const result = (id: string, text: string) =>
    row({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: text }] } })
  const inFlight = `${call('toolu_inflight_a', 'echo alpha-inflight-a')}${call('toolu_inflight_b', 'echo alpha-inflight-b')}`
  const { world, pane: opened, paneId, file, base } = await scrolledPane($, on, 'd-mvzcheee-0', sessionId, inFlight)
  let pane = opened
  let text = textOf(await drawn(pane))
  expect(text.includes('alpha-inflight-b'), `the newest in-flight call is not drawn: ${clip(text)}`).toBe(true)
  // One-row ticks up until the newest call leaves the window.
  for (let i = 0; i < 6 && text.includes('alpha-inflight-b'); i += 1) {
    await scrollPane($, paneId, -1, true)
    await world.clock.settle()
    text = textOf(await drawn(pane))
  }
  expect(text.includes('alpha-inflight-b'), 'six one-row ticks up never moved the newest call out of the window').toBe(false)
  const held = fillersIn(text, 'alpha')

  pane = await advanceUndrawn($, world, pane, {
    put: () =>
      world.put(file, `${base}${inFlight}${result('toolu_inflight_a', 'alpha-result-a')}${result('toolu_inflight_b', 'alpha-result-b')}`),
  })
  text = textOf(await drawn(pane))
  expect(text.includes('alpha-inflight-b'), 'a landed result pulled the scrolled-up window to the end').toBe(false)
  expect(fillersIn(text, 'alpha'), 'a landed result moved the scrolled-up window').toEqual(held)
  await pane.unmount()
})

test('M3c-18: a delegate list taller than the pane draws a window of it, and the wheel over the list scrolls the list alone', async ($, on) => {
  on('ui.scroll', () => ({}))
  const world = installWorld(on)
  await startSession($)
  const t = world.clock.now()
  const jobs = Array.from({ length: 50 }, (_, i) =>
    ownJob(t, `d-mvzl${String(i).padStart(4, '0')}-${i}`, `P${i}`, 60_000 * (i + 1), { sessionId: `sess-c18-${i}`, cwd: `/work/c18-${i}` }))
  world.put(transcriptPath(jobs[0]!.cwd!, jobs[0]!.sessionId!), transcriptFile('alpha', jobs[0]!.sessionId!, jobs[0]!.cwd!))
  for (const job of [...jobs].reverse()) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  const paneId = world.opens.at(-1)!.id
  const oldest = jobs.at(-1)!
  let tree = await drawn(pane)
  const listed = jobs.filter(job => entryFor(tree, job.id) !== undefined).length
  // The body holds the pane frame's two rows and the list's heading row
  // beside the entries.
  expect(listed > 0 && listed <= PANE_PROPS.scroll.bodyRows - 3, `the list draws ${listed} entries into a ${PANE_PROPS.scroll.bodyRows}-row body`).toBe(true)
  expect(entryFor(tree, oldest.id), 'the oldest delegate already shows before any scroll').toBe(undefined)
  const transcriptBefore = fillersIn(textOf(tree), 'alpha')

  await $.ui.scroll({
    component: 'Pane',
    requestId: paneId,
    offset: 0,
    by: 200,
    bodyRows: PANE_PROPS.scroll.bodyRows,
    contentRows: PANE_PROPS.scroll.bodyRows,
    origin: { kind: 'person' },
    pointer: { column: 1, row: Math.floor(PANE_PROPS.scroll.bodyRows / 2) },
  })
  await world.clock.settle()
  tree = await drawn(pane)
  expect(entryFor(tree, oldest.id) !== undefined, 'a wheel over the list never brings its oldest delegate into view').toBe(true)
  expect(fillersIn(textOf(tree), 'alpha'), 'a wheel over the list moved the transcript').toEqual(transcriptBefore)
  await pane.unmount()
})

test('M3c-11: the transcript draws tool calls as Claude Code does, home as `~`, and the delegate\'s messages in untitled cards, on clauth\'s background', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzcbaaa-0', 'D1', 300_000, { sessionId: 'sess-c11', cwd: '/work/alpha' })
  world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript('alpha', job.sessionId!, job.cwd!))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountTallPane($, world)
  const tree = await drawn(pane)
  const text = textOf(tree)

  expect(isDrawn(tree) && tree.props?.backgroundColor === PANE_BG, `the pane is not drawn on ${PANE_BG}: ${clip(JSON.stringify(isDrawn(tree) ? tree.props : tree) ?? '')}`).toBe(true)
  expect(glyphLeaves(tree, ['⏺']).length >= 2, `the transcript draws no ⏺ per tool call: ${clip(text)}`).toBe(true)
  expect(text.includes('Bash(cargo test)'), `the Bash call is not drawn as Bash(cargo test): ${clip(text)}`).toBe(true)
  expect(text.includes('Read(~/work/alpha/code.md)'), `the Read call is not drawn with home as ~: ${clip(text)}`).toBe(true)
  expect(text.includes(`${HOME}/work`), `the transcript spells out the home directory: ${clip(text)}`).toBe(false)
  const result = leafWith(tree, 'alpha 12 passed')
  expect(result !== undefined, `the Bash call's result line is not drawn: ${clip(text)}`).toBe(true)
  expect(
    elements(tree).some(el => elements(el).includes(result!) && /⎿\s*alpha 12 passed/.test(textOf(el))),
    `the Bash call's result line has no ⎿ lead: ${clip(text)}`,
  ).toBe(true)
  expect(text.includes('{"command"'), `a tool call is drawn as raw JSON: ${clip(text)}`).toBe(false)

  const card = elements(tree)
    .filter(el => el.type === 'Box' && typeof el.props?.borderStyle === 'string' && hasWord(textOf(el), 'alpha-reply-final'))
    .at(-1)
  expect(card !== undefined, `the delegate's message is not drawn in an outlined card: ${clip(text)}`).toBe(true)
  expect(hasWord(textOf(card), DELEGATE_MODEL), `the message card is titled by its model: ${clip(textOf(card))}`).toBe(false)
  expect(text.split(DELEGATE_MODEL).length - 1, `the model is drawn more than once, outside the header: ${clip(text)}`).toBe(1)
  await pane.unmount()
})

test('M3c-11b: a message wraps whole with its markdown drawn, a path under the delegate\'s cwd is relative, and results read as Claude Code summarizes them', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzcbbbb-0', 'D1', 300_000, { sessionId: 'sess-c11b', cwd: '/work/alpha' })
  world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript('alpha', job.sessionId!, job.cwd!))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountTallPane($, world)
  const tree = await drawn(pane)
  const text = textOf(tree)

  expect(hasWord(text, 'alpha-long-tail'), `a long message is cut instead of wrapped: ${clip(text)}`).toBe(true)
  expect(text.includes('**'), `a message's markdown is drawn raw: ${clip(text)}`).toBe(false)
  expect(text.includes('Read(src/lib.ts)'), `a file under the delegate's cwd is not drawn relative to it: ${clip(text)}`).toBe(true)
  expect(text.includes('/work/alpha/src'), `a path under the delegate's cwd is drawn whole: ${clip(text)}`).toBe(false)
  expect(/⎿\s*Read 5 lines\b/.test(text), `a Read result is not summarized as Read 5 lines: ${clip(text)}`).toBe(true)
  for (const shown of ['alpha-out-1', 'alpha-out-2', 'alpha-out-3']) {
    expect(hasWord(text, shown), `a Bash result's first lines leave out ${shown}: ${clip(text)}`).toBe(true)
  }
  for (const hidden of ['alpha-out-4', 'alpha-out-5', 'alpha-out-6']) {
    expect(hasWord(text, hidden), `a Bash result draws past its first 3 lines: ${hidden}`).toBe(false)
  }
  expect(/…\s*\+3 lines\b/.test(text), `a Bash result does not count its 3 hidden lines: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3c-11c: a fenced code block in a delegate\'s message draws its code as written, without the fence lines or inline marks', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const cwd = '/work/alpha'
  const job = ownJob(world.clock.now(), 'd-mvzcbccc-0', 'D1', 300_000, { sessionId: 'sess-c11c', cwd })
  const fenced = 'alpha-fence-intro\n```ts\nconst total = a*b*c + **x**\n```\n**alpha-fence-outro**'
  world.put(
    transcriptPath(cwd, job.sessionId!),
    `${delegateTranscript('alpha', job.sessionId!, cwd)}${JSON.stringify({
      type: 'assistant',
      message: { role: 'assistant', model: DELEGATE_MODEL, content: [{ type: 'text', text: fenced }] },
      sessionId: job.sessionId,
      cwd,
    })}\n`,
  )
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountTallPane($, world)
  const tree = await drawn(pane)
  const text = textOf(tree)
  expect(hasWord(text, 'alpha-fence-intro'), `the fenced message is not drawn: ${clip(text)}`).toBe(true)
  expect(text.includes('```'), `a code fence is drawn raw: ${clip(text)}`).toBe(false)
  const code = elements(tree).filter(el => el.type === 'Text' && textOf(el).includes('const total = a*b*c + **x**')).at(-1)
  expect(code !== undefined, `the code line is not drawn as written: ${clip(text)}`).toBe(true)
  expect(elements(code).some(el => el.props?.bold === true || el.props?.italic === true), 'inline marks style the inside of a code block').toBe(false)
  // The closing fence ends the block: the line after it draws its marks.
  const outro = leafWith(tree, 'alpha-fence-outro')
  expect(outro !== undefined && outro.props?.bold === true, `the line after a code block lost its bold: ${clip(text)}`).toBe(true)
  await pane.unmount()
})

test('M3c-12: under a dialog that takes the keys the pane draws none of its delegates, and draws them again once it closes', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const words = [...jobs.map(j => j.profile), 'alpha-reply-final']

  const covered = await mountCoveredPane($, world, true)
  const hidden = textOf(await drawn(covered))
  for (const word of words) {
    expect(hasWord(hidden, word), `the pane draws ${word} under a dialog: ${clip(hidden)}`).toBe(false)
  }
  await covered.unmount()

  const uncovered = await mountCoveredPane($, world, false)
  const shown = textOf(await drawn(uncovered))
  for (const word of words) {
    expect(hasWord(shown, word), `the pane does not draw ${word} once the dialog closed: ${clip(shown)}`).toBe(true)
  }
  await uncovered.unmount()
})

test('M3c-15: a delegate call opens no pane by itself, while the band\'s count still opens it', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzcfaaa-0', 'D1', 1_000)
  await delegate($, world, job)
  await world.clock.settle()
  await world.clock.advance(POLL_MS)
  expect(world.opens.length, 'a delegate call opened the delegates pane by itself').toBe(0)
  const band = await mountBand($)
  await band.press({ key: keyOf(countControl(await bandFrame(band))!) })
  await world.clock.settle()
  expect(world.opens.length, 'the band\'s count did not open the delegates pane').toBe(1)
  await band.unmount()
})

const DIALOG_QUESTIONS = [
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

// A question or permission prompt closes the delegates pane (not only its
// body), and answering it reopens the pane on the delegate it showed.
for (const dialog of ['permission', 'question'] as const) {
  test(`M3c-14: a ${dialog} dialog closes the delegates pane, and answering it reopens the pane on the delegate it showed`, async ($, on) => {
    let answer: () => void = () => {}
    const waitForAnswer = () => new Promise<void>(resolve => {
      answer = resolve
    })
    if (dialog === 'permission') {
      on('tool.check', ($, e, next) => (e.tool === 'Bash' ? { decision: 'ask' as const } : next(e)))
      on('classic.PermissionRequest', async () => {
        await waitForAnswer()
        return { decision: { behavior: 'allow' as const } }
      })
    } else {
      on('tool.call', { tool: 'AskUserQuestion' }, async () => {
        await waitForAnswer()
        return { result: { answers: { 'Which lib?': 'date-fns' } } } as ToolCallResult
      })
    }
    const world = installWorld(on)
    installCommands(on)
    await startSession($)
    const t = world.clock.now()
    const older = ownJob(t, 'd-mvzceaaa-0', 'D1', 600_000, { sessionId: 'sess-c14-alpha', cwd: '/work/alpha' })
    const newer = ownJob(t, 'd-mvzcebbb-1', 'D4', 60_000, { sessionId: 'sess-c14-gamma', cwd: '/work/gamma' })
    world.put(transcriptPath(older.cwd!, older.sessionId!), delegateTranscript('alpha', older.sessionId!, older.cwd!))
    world.put(transcriptPath(newer.cwd!, newer.sessionId!), delegateTranscript('gamma', newer.sessionId!, newer.cwd!))
    await delegate($, world, older)
    await delegate($, world, newer)
    await world.clock.advance(POLL_MS)
    await $.command.run({ command: 'delegates' })
    await world.clock.settle()
    const paneId = world.opens.at(-1)!.id
    let pane = await mountPane($, world)
    const entry = buttonsIn(await drawn(pane)).find(b => keyOf(b) === older.id)
    expect(entry !== undefined, `the pane lists no entry keyed ${older.id}`).toBe(true)
    await pane.press({ key: keyOf(entry!) })
    pane = await advanceUndrawn($, world, pane)
    expect(hasWord(textOf(await drawn(pane)), 'alpha-reply-final'), 'choosing the older delegate did not show it').toBe(true)
    await pane.unmount()

    const call = dialog === 'permission'
      ? $.tool.call({ tool: 'Bash', command: 'ls' })
      : $.tool.call({ tool: 'AskUserQuestion', questions: DIALOG_QUESTIONS })
    await world.clock.settle()
    expect(world.open.has(paneId), `the delegates pane stayed open under the ${dialog} dialog`).toBe(false)

    answer()
    await call
    await world.clock.settle()
    expect(world.open.has(paneId), `answering the ${dialog} dialog did not reopen the delegates pane`).toBe(true)
    pane = await mountPane($, world)
    const text = textOf(await drawn(pane))
    expect(hasWord(text, 'alpha-reply-final'), `the reopened pane is not on the delegate it showed: ${clip(text)}`).toBe(true)
    expect(hasWord(text, 'gamma-reply-final'), 'the reopened pane fell back to the newest delegate').toBe(false)
    await pane.unmount()
  })
}

test('M3c-14c: answering a permission prompt reopens the pane on the delegate it showed even when a store tick lands during the reopen', { plugins: [HOLD_REOPEN_ALPHA_READ] }, async ($, on) => {
  let answer: () => void = () => {}
  const answered = new Promise<void>(resolve => {
    answer = resolve
  })
  on('tool.check', ($, e, next) => (e.tool === 'Bash' ? { decision: 'ask' as const } : next(e)))
  on('classic.PermissionRequest', async () => {
    await answered
    return { decision: { behavior: 'allow' as const } }
  })
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const t = world.clock.now()
  const older = ownJob(t, 'd-mvzceddd-0', 'D1', 600_000, { sessionId: 'sess-c14c-alpha', cwd: '/work/alpha' })
  const newer = ownJob(t, 'd-mvzceeee-1', 'D4', 60_000, { sessionId: 'sess-c14c-gamma', cwd: '/work/gamma' })
  world.put(transcriptPath(older.cwd!, older.sessionId!), delegateTranscript('alpha', older.sessionId!, older.cwd!))
  world.put(transcriptPath(newer.cwd!, newer.sessionId!), delegateTranscript('gamma', newer.sessionId!, newer.cwd!))
  await delegate($, world, older)
  await delegate($, world, newer)
  await world.clock.advance(POLL_MS)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const paneId = world.opens.at(-1)!.id
  let pane = await mountPane($, world)
  await pane.press({ key: keyOf(entryFor(await drawn(pane), older.id)!) })
  pane = await advanceUndrawn($, world, pane)
  expect(hasWord(textOf(await drawn(pane)), 'alpha-reply-final'), 'choosing the older delegate did not show it').toBe(true)
  await pane.unmount()

  const call = $.tool.call({ tool: 'Bash', command: 'ls' })
  await world.clock.settle()
  expect(world.open.has(paneId), 'the delegates pane stayed open under the permission prompt').toBe(false)
  answer()
  await call
  await world.clock.settle()
  expect(heldReads(world, 'sess-c14c-alpha'), 'the reopen\'s transcript read was not held').toBe(1)
  await world.clock.advance(POLL_MS)
  expect(world.open.has(paneId), 'answering the permission prompt did not reopen the pane').toBe(true)
  pane = await mountPane($, world)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-reply-final'), `a store tick during the reopen put the pane on another delegate: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'gamma-reply-final'), 'the reopened pane fell back to the newest delegate').toBe(false)
  await pane.unmount()
})

// A permission prompt guarding a slow tool: the pane comes back once the
// prompt is answered, not once the tool it allowed finishes its run.
test('M3c-14b: answering a permission prompt reopens the delegates pane while the tool it allowed is still running', async ($, on) => {
  let answer: () => void = () => {}
  const answered = new Promise<void>(resolve => {
    answer = resolve
  })
  let finish: () => void = () => {}
  const finished = new Promise<void>(resolve => {
    finish = resolve
  })
  on('tool.check', ($, e, next) => (e.tool === 'Bash' ? { decision: 'ask' as const } : next(e)))
  on('classic.PermissionRequest', async () => {
    await answered
    return { decision: { behavior: 'allow' as const } }
  })
  on('tool.call', { tool: 'Bash' }, async () => {
    await finished
    return { result: 'ok' } as ToolCallResult
  })
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzceccc-0', 'D1', 60_000, { sessionId: 'sess-c14b', cwd: '/work/alpha' })
  world.put(transcriptPath(job.cwd!, job.sessionId!), delegateTranscript('alpha', job.sessionId!, job.cwd!))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const paneId = world.opens.at(-1)!.id

  const call = $.tool.call({ tool: 'Bash', command: 'cargo build' })
  await world.clock.settle()
  expect(world.open.has(paneId), 'the delegates pane stayed open under the permission prompt').toBe(false)

  answer()
  await world.clock.settle()
  expect(world.open.has(paneId), 'answering the permission prompt did not reopen the pane while the tool still ran').toBe(true)

  finish()
  await call
  await world.clock.settle()
  expect(world.open.has(paneId), 'the pane closed again once the tool finished').toBe(true)
})

// The rows a drawn element lays out as, in the tree's model: a bare string
// or a Text one row, a column Box its children's rows capped by its height
// when it hides the overflow, a borderStyle Box two border rows more.
function rowsOf(x: unknown): number {
  if (typeof x === 'string' || typeof x === 'number') return 1
  if (!isDrawn(x)) return 0
  let rows = (x.children ?? []).reduce((sum: number, child) => sum + rowsOf(child), 0)
  if (typeof x.props?.borderStyle === 'string') rows += 2
  const height = x.props?.height
  if (typeof height === 'number' && x.props?.overflow === 'hidden') rows = Math.min(rows, height)
  return rows
}

// The element a cut card is drawn as, holding a word: its text rows are
// Texts, so the innermost Box whose text holds the word is the card itself.
function cutCardElementFor(root: unknown, word: string): Drawn | undefined {
  return elements(root)
    .filter(el => el.type === 'Box' && hasWord(textOf(el), word))
    .at(-1)
}

// The element a whole card is drawn as: the borderStyle Box holding a word
// (a whole card nests one content Box inside its frame).
function wholeCardElementFor(root: unknown, word: string): Drawn | undefined {
  return elements(root)
    .filter(el => el.type === 'Box' && typeof el.props?.borderStyle === 'string' && hasWord(textOf(el), word))
    .at(-1)
}

test('M3c-17g: a card cut at either window edge draws exactly the rows the window covers, its borders kept at the cut', async ($, on) => {
  const sessionId = 'sess-c17g'
  const cwd = '/work/alpha'
  let n = 0
  let tailRows = 0
  const said = (text: string) => {
    n += 1
    // Every fixture line fits the card's width in one row, so a said
    // message draws its two border rows plus its line count.
    tailRows += 2 + text.split('\n').length
    return `${JSON.stringify({ type: 'assistant', message: { role: 'assistant', content: [{ type: 'text', text }] }, sessionId, cwd, uuid: `c17g-u${n}` })}\n`
  }
  // The tail's layout, 97 rows: 11 one-line cards (33), a three-line card at
  // rows 33-37, four four-line and one three-line card (29), then ten
  // one-line cards (30) — `alpha-under-0` at rows 67-69.
  const tail = [
    ...Array.from({ length: 11 }, (_, i) => said(`alpha-fill-${i}`)),
    said('alpha-topcut-1\nalpha-topcut-2\nalpha-topcut-3'),
    said('alpha-mid-a-1\nalpha-mid-a-2\nalpha-mid-a-3\nalpha-mid-a-4'),
    said('alpha-mid-b-1\nalpha-mid-b-2\nalpha-mid-b-3\nalpha-mid-b-4'),
    said('alpha-mid-c-1\nalpha-mid-c-2\nalpha-mid-c-3\nalpha-mid-c-4'),
    said('alpha-mid-d-1\nalpha-mid-d-2\nalpha-mid-d-3\nalpha-mid-d-4'),
    said('alpha-mid-e-1\nalpha-mid-e-2\nalpha-mid-e-3'),
    ...Array.from({ length: 10 }, (_, i) => said(`alpha-under-${i}`)),
  ].join('')
  expect(tailRows, 'the fixture is not the 97 rows it was laid out as').toBe(97)
  const { world, pane, paneId } = await scrolledPane($, on, 'd-mvzchhhh-0', sessionId, tail)
  // 28 one-row ticks up from the end: the window covers tail rows 36-68,
  // cutting the three-line card above its last text row and `alpha-under-0`
  // below its first.
  const ticks = 28
  for (let i = 0; i < ticks; i += 1) await scrollPane($, paneId, -1, true)
  await world.clock.settle()
  const tree = await drawn(pane)
  const transcriptRows = PANE_PROPS.scroll.bodyRows - 2 - 5
  const top = tailRows - transcriptRows - ticks
  const bottom = top + transcriptRows
  expect(top, 'the window does not start on the three-line card').toBe(36)
  const rowsGiven = (start: number, rows: number) => Math.min(rows, bottom - start) - Math.max(0, top - start)

  const topcut = cutCardElementFor(tree, 'alpha-topcut-3')
  expect(topcut, 'the window cut no card at its top: no element holds alpha-topcut-3').toBeDefined()
  const under0 = cutCardElementFor(tree, 'alpha-under-0')
  expect(under0, 'a card cut at the window\'s bottom drew nothing: no element holds alpha-under-0').toBeDefined()
  const whole = wholeCardElementFor(tree, 'alpha-mid-e-1')
  expect(whole, 'the window lost the whole mid card').toBeDefined()
  expect(rowsOf(topcut!), `the top-cut card draws ${rowsOf(topcut!)} rows, the window gives it ${rowsGiven(33, 5)}`).toBe(rowsGiven(33, 5))
  expect(rowsOf(under0!), `the bottom-cut card draws ${rowsOf(under0!)} rows, the window gives it ${rowsGiven(67, 3)}`).toBe(rowsGiven(67, 3))
  expect(rowsOf(whole!), `a whole card draws ${rowsOf(whole!)} rows, the window gives it ${rowsGiven(62, 5)}`).toBe(rowsGiven(62, 5))

  // The cut cards' text rows are the ones the window covers: the row above
  // the top cut and the row below the bottom cut stay out.
  expect(hasWord(textOf(topcut!), 'alpha-topcut-2'), 'the top-cut card drew the text row above the cut').toBe(false)
  expect(hasWord(textOf(topcut!), 'alpha-topcut-3'), 'the top-cut card lost the text row the window covers').toBe(true)
  expect(hasWord(textOf(under0!), 'alpha-under-0'), 'the bottom-cut card lost the text row the window covers').toBe(true)
  expect(hasWord(textOf(under0!), 'alpha-under-1'), 'the bottom-cut card drew the row below the cut').toBe(false)

  // A cut card keeps its covered border row and drops the cut one; a whole
  // card's borders come from its borderStyle, no hand-drawn row.
  const borderRow = (card: Drawn, glyph: '╭' | '╰'): Drawn | undefined =>
    elements(card).find(el => el.type === 'Text' && textOf(el).startsWith(glyph))
  expect(borderRow(topcut!, '╰'), 'the top-cut card lost its closing border').toBeDefined()
  expect(borderRow(topcut!, '╭'), 'the top-cut card drew its cut-away top border').toBeUndefined()
  expect(borderRow(under0!, '╭'), 'the bottom-cut card lost its top border').toBeDefined()
  expect(borderRow(under0!, '╰'), 'the bottom-cut card drew its cut-away closing border').toBeUndefined()
  expect(borderRow(whole!, '╭'), 'a whole card drew a hand-made border row').toBeUndefined()
  const cardWidth = PANE_PROPS.bodyColumns - Math.min(40, Math.max(12, Math.floor(PANE_PROPS.bodyColumns * 0.32))) - 8
  expect(textOf(borderRow(under0!, '╭')!).length, 'a cut card\'s border does not span the card\'s width').toBe(cardWidth)

  // The cut cards' text keeps the whole card's one-column padding.
  const textRow = (card: Drawn, word: string): Drawn | undefined =>
    elements(card).find(el => el.type === 'Text' && hasWord(textOf(el), word))
  expect(textRow(under0!, 'alpha-under-0')?.children?.[0], 'the bottom-cut card\'s text lost its padding column').toBe(' ')
  expect(textRow(topcut!, 'alpha-topcut-3')?.children?.[0], 'the top-cut card\'s text lost its padding column').toBe(' ')
  await pane.unmount()
})

// One choice's final write to `delegateView` throws (the engine's CAS miss:
// update re-reads and tries again to its bound, then throws): the guard must
// still settle, so the next tick's refresh makes its own choice again
// instead of skipping for the pane's life.
test('M3c-8d: a choice whose view write throws settles its guard, so the next tick still chooses the newest delegate', async ($, on) => {
  let refusing = false
  on('state.set', { plugin: 'clauth', key: 'delegateView' }, ($, e, next) => {
    if (refusing) {
      // The engine's compare-and-set miss: update re-reads and tries again up
      // to its bound, then throws and nothing is written.
      return { value: { isSet: false, version: 0 } }
    }
    return next(e)
  })
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)

  refusing = true
  const result = await $.command.run({ command: 'delegates' })
  refusing = false
  expect(result.text, 'the throwing view write failed the open although the pane opened').toBe('delegates pane open')
  expect(
    world.notices.filter(notice => notice.via === 'toast' && notice.text === 'could not show a delegate in the pane').length,
    'the throwing view write left the person no toast saying the pane shows no delegate',
  ).toBe(1)
  await world.clock.settle()
  let pane = await mountPane($, world)
  expect(hasWord(textOf(await drawn(pane)), 'alpha-reply-final'), 'the pane chose a delegate despite the throwing write').toBe(false)

  // The choice's throw settled its guard: the next tick's refresh lands on
  // the newest delegate again.
  pane = await advanceUndrawn($, world, pane, { ms: TICK_MS })
  await world.clock.settle()
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-reply-final'), `a thrown choice froze the refresh's no-view branch: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'gamma-reply-final'), 'the recovered choice picked a delegate other than the newest').toBe(false)
  await pane.unmount()
})

// The engine raises a page key against the tree it laid out, which here fits
// the body: `by` is the body's rows with no pointer (Home and End the same).
// The transcript's own window is smaller by the header, the status line and
// their blank rows, so a page key must move it by that window, not the body's.
test('M3c-19: a page key moves the transcript by exactly the window it shows, so two pages down skip no row', async ($, on) => {
  // The engine beneath: its window over a tree that fits stays where it is.
  on('ui.scroll', () => ({}))
  const world = installWorld(on)
  await startSession($)
  const sessionId = 'sess-c19'
  const cwd = '/work/alpha'
  const row = (o: Record<string, unknown>) => `${JSON.stringify({ sessionId, cwd, version: '2.1.325', isSidechain: false, ...o })}\n`
  // 150 tool calls with no result, one drawn row each: at the top the window
  // shows calls 0 to N-1, so its first row is a call's own number and a page
  // down must land on call N.
  const rows = [
    row({ type: 'user', message: { role: 'user', content: 'alpha: page through the transcript' }, uuid: 'c19-u0' }),
    ...Array.from({ length: 150 }, (_, i) =>
      row({
        type: 'assistant',
        message: { role: 'assistant', model: DELEGATE_MODEL, content: [{ type: 'tool_use', id: `toolu_c19_${i}`, name: 'Bash', input: { command: `echo alpha-pg-${i}` } }] },
        uuid: `c19-u${i + 1}`,
      })),
  ]
  const job = ownJob(world.clock.now(), 'd-mvzc19aa-0', 'D1', 300_000, { sessionId, cwd })
  world.put(transcriptPath(cwd, sessionId), rows.join(''))
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  const paneId = world.opens.at(-1)!.id
  const callsShown = (text: string): number[] =>
    [...text.matchAll(/(?<![A-Za-z0-9_-])alpha-pg-(\d+)(?![A-Za-z0-9_-])/g)].map(m => Number(m[1]))

  // The top of the transcript: a wheel move far up, which keeps its own rows.
  await scrollPane($, paneId, -100_000, true)
  await world.clock.settle()
  const top = callsShown(textOf(await drawn(pane)))
  expect(top[0], `the top window does not start on the transcript's first row: ${top.join(',')}`).toBe(0)
  const windowRows = top.length
  expect(3 * windowRows < 150, `the fixture is not taller than three windows of ${windowRows} rows`).toBe(true)

  // One page down: the first row shown is one window below the first row the
  // top window showed.
  await scrollPane($, paneId, PANE_PROPS.scroll.bodyRows, false)
  await world.clock.settle()
  const first = callsShown(textOf(await drawn(pane)))
  expect(first[0], `a page down landed on row ${first[0]}, not the ${windowRows} rows below the top`).toBe(windowRows)
  // A second page down: two windows below the top.
  await scrollPane($, paneId, PANE_PROPS.scroll.bodyRows, false)
  await world.clock.settle()
  const second = callsShown(textOf(await drawn(pane)))
  expect(second[0], `a second page down landed on row ${second[0]}, not ${2 * windowRows} below the top`).toBe(2 * windowRows)
  await pane.unmount()
})

// A finished chip's choice is held while a store tick lands: the tick's
// refresh sees no view yet and would choose the newest delegate of its own, so
// the press's choice must keep it out until it writes.
const HOLD_FIRST_BETA_READ: Plugin = {
  name: 'hold-first-beta-read',
  register(on) {
    let armed = true
    on('process.run', async ($, e, next) => {
      if (armed && e.argv[0] === 'tail' && String(e.argv[e.argv.length - 1] ?? '').includes('sess-c8-beta')) {
        armed = false
        $.ui.toast('held sess-c8-beta')
        await $.clock.sleep(5_000)
      }
      return next(e)
    })
  },
}

test('M3c-8e: a finished chip pressed while delegates run opens the pane on its delegate even when a store tick lands during the choice', { plugins: [HOLD_FIRST_BETA_READ] }, async ($, on) => {
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const jobs = paneBatch(world)
  for (const job of jobs) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let band = await mountBand($)
  const chip = chipFor(await bandFrame(band), 'zcode', jobs.map(j => j.profile))
  expect(chip !== undefined, 'the band holds no chip for the finished zcode').toBe(true)
  await band.press({ key: keyOf(chip!) })
  await world.clock.settle()
  expect(heldReads(world, 'sess-c8-beta'), 'the beta transcript read was not held').toBe(1)
  band = await advanceBandUndrawn($, world, band)
  const pane = await mountPane($, world)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'beta-reply-final'), `the pane opened from the finished zcode chip does not show its transcript: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), 'a store tick during the finished chip\'s choice put the pane on the newest delegate').toBe(false)
  await pane.unmount()
  await band.unmount()
})

test('M3c-14d: answering a question reopens the pane on the delegate it showed even when a store tick lands during the reopen', { plugins: [HOLD_REOPEN_ALPHA_READ] }, async ($, on) => {
  let answer: () => void = () => {}
  const answered = new Promise<void>(resolve => {
    answer = resolve
  })
  on('tool.call', { tool: 'AskUserQuestion' }, async () => {
    await answered
    return { result: { answers: { 'Which lib?': 'date-fns' } } } as ToolCallResult
  })
  const world = installWorld(on)
  installCommands(on)
  await startSession($)
  const t = world.clock.now()
  const older = ownJob(t, 'd-mvzcpaaa-0', 'D1', 600_000, { sessionId: 'sess-c14c-alpha', cwd: '/work/alpha' })
  const newer = ownJob(t, 'd-mvzcpbbb-1', 'D4', 60_000, { sessionId: 'sess-c14c-gamma', cwd: '/work/gamma' })
  world.put(transcriptPath(older.cwd!, older.sessionId!), delegateTranscript('alpha', older.sessionId!, older.cwd!))
  world.put(transcriptPath(newer.cwd!, newer.sessionId!), delegateTranscript('gamma', newer.sessionId!, newer.cwd!))
  await delegate($, world, older)
  await delegate($, world, newer)
  await world.clock.advance(POLL_MS)
  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const paneId = world.opens.at(-1)!.id
  let pane = await mountPane($, world)
  await pane.press({ key: keyOf(entryFor(await drawn(pane), older.id)!) })
  pane = await advanceUndrawn($, world, pane)
  expect(hasWord(textOf(await drawn(pane)), 'alpha-reply-final'), 'choosing the older delegate did not show it').toBe(true)
  await pane.unmount()

  const call = $.tool.call({ tool: 'AskUserQuestion', questions: DIALOG_QUESTIONS })
  await world.clock.settle()
  expect(world.open.has(paneId), 'the delegates pane stayed open under the question').toBe(false)
  answer()
  await call
  await world.clock.settle()
  expect(heldReads(world, 'sess-c14c-alpha'), 'the reopen\'s transcript read was not held').toBe(1)
  await world.clock.advance(POLL_MS)
  expect(world.open.has(paneId), 'answering the question did not reopen the pane').toBe(true)
  pane = await mountPane($, world)
  const text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-reply-final'), `a store tick during the reopen put the pane on another delegate: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'gamma-reply-final'), 'the reopened pane fell back to the newest delegate').toBe(false)
  await pane.unmount()
})
