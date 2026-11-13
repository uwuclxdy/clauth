// The delegates pane of the clauth mod (M3a): this session's delegates read
// off clauth's job files, a delegate's own transcript, stop through clauth's
// `monitor` tool, auto-open on a `delegate` call, finish/cancel notices, and
// finished rows kept for the session.
//
// The tests pin data and structure, never copy: job ids, accounts, states,
// one row per job, one notice per job, the exact `monitor` arguments. Where a
// control has to be found, a word presence check stands in for its label
// (`stop`/`cancel` for the stop action); everything else is found by the job
// id it carries. The pane's id is whatever the mod hands `$.ui.open`.
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
  // run over a transcript (`tail`, `cat`), answered from the files above.
  on('process.run', ($, e) => {
    const argv = e.argv
    if (argv[0] === 'clauth') return processResult(0, '')
    if (argv[0] === 'tail' || argv[0] === 'cat') {
      const path = argv[argv.length - 1] ?? ''
      const f = files.get(norm(path))
      if (f === undefined) return processResult(1, '', `${argv[0]}: ${path}: No such file or directory`)
      if (argv[0] === 'cat') return processResult(0, f.text)
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
  // once here, before a test's delegate call: the unasked open reads the fact
  // the engine would already have handed it.
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

// One job's row: the outermost element showing that job's id and no other
// fixture job's id.
function rowFor(root: unknown, id: string, allIds: readonly string[]): Drawn | undefined {
  return elements(root).find(el => {
    const text = textOf(el)
    return hasWord(text, id) && allIds.every(other => other === id || !hasWord(text, other))
  })
}

const buttonsIn = (root: unknown): Drawn[] => elements(root).filter(el => el.type === 'Button')

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

// The pane the mod opened, mounted where a docked pane draws. The id is the
// one the mod handed `$.ui.open`; a mod that opened none fails here.
async function mountPane($: Engine, world: World): Promise<Pane> {
  const opened = world.opens.at(-1)
  expect(opened !== undefined, 'the mod opened no pane: a delegate call opens the delegates pane').toBe(true)
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

// Presses a row's own control: the first Button in the row that is not the
// stop action, then lets any read the press started finish.
async function openRow(pane: Pane, world: World, id: string, allIds: readonly string[]): Promise<void> {
  const row = rowFor(await drawn(pane), id, allIds)
  expect(row !== undefined, `the pane draws no row for ${id}`).toBe(true)
  const control = buttonsIn(row).find(b => !STOP_WORD.test(textOf(b)))
  expect(control !== undefined, `the row for ${id} holds no Button that opens it`).toBe(true)
  await pane.press({ key: keyOf(control!) })
  await world.clock.advance(POLL_MS)
}

// From a delegate's transcript view back to the list: presses each Button
// that is not the stop action until every listed job's row shows again and
// the transcript's lines are gone. True when one did.
async function backToList(
  pane: Pane,
  world: World,
  listed: readonly string[],
  viewWords: readonly string[],
): Promise<boolean> {
  const isList = (text: string) =>
    listed.every(id => hasWord(text, id)) && viewWords.every(word => !hasWord(text, word))
  for (const button of buttonsIn(await drawn(pane)).filter(b => !STOP_WORD.test(textOf(b)))) {
    try {
      await pane.press({ key: keyOf(button) })
    } catch {
      continue
    }
    await world.clock.settle()
    if (isList(textOf(await drawn(pane)))) return true
  }
  return false
}

// Presses the stop action for a running job: the one in its row, else the one
// in its transcript view.
async function pressStop(pane: Pane, world: World, id: string, allIds: readonly string[]): Promise<void> {
  const row = rowFor(await drawn(pane), id, allIds)
  expect(row !== undefined, `the pane draws no row for running ${id}`).toBe(true)
  let stop = buttonsIn(row).find(b => STOP_WORD.test(textOf(b)))
  if (stop === undefined) {
    await openRow(pane, world, id, allIds)
    stop = buttonsIn(await drawn(pane)).find(b => STOP_WORD.test(textOf(b)))
  }
  expect(stop !== undefined, `neither the row nor the transcript view of running ${id} offers a stop action`).toBe(true)
  await pane.press({ key: keyOf(stop!) })
  await world.clock.settle()
}

// How many notices (toast or native notification) name the job so far.
function noticesNaming(world: World, id: string): number {
  return world.notices.filter(n => hasWord(n.text, id)).length
}

// The checks one row of the list carries: its account, its state word and
// its elapsed time, read against the clock. A running row's elapsed time is
// the time since `started_at`, at most one poll stale; a finished row's lies
// between its run's length and the time since it started.
function expectRow(root: unknown, job: Job, allIds: readonly string[], now: number): void {
  const row = rowFor(root, job.id, allIds)
  expect(row !== undefined, `the pane draws no row for this session's ${job.id}`).toBe(true)
  const text = textOf(row)
  expect(hasWord(text, job.profile), `the row for ${job.id} does not name its account ${job.profile}: ${clip(text)}`).toBe(true)
  const sinceStart = Math.floor((now - job.startedAt) / 1000)
  const pollSecs = POLL_MS / 1000
  if (job.state === 'running') {
    expect(RUNNING_WORD.test(text), `the row for running ${job.id} does not say running: ${clip(text)}`).toBe(true)
    const fits = durationsIn(text).some(s => s >= sinceStart - pollSecs - 1 && s <= sinceStart + 1)
    expect(fits, `the row for ${job.id} shows no elapsed time near ${sinceStart}s: ${clip(text)}`).toBe(true)
  } else {
    expect(FINISHED_WORD.test(text), `the row for finished ${job.id} does not say done: ${clip(text)}`).toBe(true)
    expect(RUNNING_WORD.test(text), `the row for finished ${job.id} still says running: ${clip(text)}`).toBe(false)
    const ran = Math.floor(((job.doneAt ?? now) - job.startedAt) / 1000)
    const fits = durationsIn(text).some(s => s >= ran - pollSecs - 1 && s <= sinceStart + 1)
    expect(fits, `the row for ${job.id} shows no elapsed time between ${ran}s and ${sinceStart}s: ${clip(text)}`).toBe(true)
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
  const tree = await drawn(pane)
  const allIds = [...own.map(j => j.id), ...foreign.map(j => j.id), foreignDelivered]
  for (const job of own) expectRow(tree, job, allIds, world.clock.now())
  const whole = textOf(tree)
  for (const id of [...foreign.map(j => j.id), foreignDelivered]) {
    expect(hasWord(whole, id), `the pane lists ${id}, a delegate of another session`).toBe(false)
  }
  await pane.unmount()
})

// --- 2. auto-open ---

test('M3a-2a: a completed delegate call opens the pane; a call of any other tool does not', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  await $.tool.call({ tool: 'Bash', command: 'ls' })
  await $.tool.call({ tool: 'mcp__plugin_clauth_clauth__profiles' })
  await $.tool.call({ tool: 'mcp__plugin_clauth_clauth__monitor', job_ids: [] })
  await $.tool.call({ tool: 'mcp__plugin_other_other__delegate', prompt: 'not clauth' })
  await world.clock.advance(POLL_MS)
  expect(world.opens.length, 'a pane opened before any clauth delegate call').toBe(0)

  await delegate($, world, ownJob(world.clock.now(), 'd-mvzf2aaa-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a completed clauth delegate call did not open the pane exactly once').toBe(1)
  expect(world.open.has(world.opens[0]!.id), 'the pane the delegate call opened is not open').toBe(true)
})

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

test('M3a-2b: a delegate call after the pane was closed opens it again', { plugins: [PANE_CLOSER] }, async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzf2bbb-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a completed clauth delegate call did not open the pane').toBe(1)
  const id = world.opens[0]!.id

  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(id), 'the pane stayed open after it was closed').toBe(false)
  await world.clock.advance(POLL_MS)
  const opensAfterClose = world.opens.length
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzf2bbb-1', 'D2', 1_000))
  await world.clock.settle()
  expect(world.opens.length - opensAfterClose, 'a delegate call after the person closed the pane did not open it again').toBe(1)
  expect(world.opens.at(-1)!.id, 'the delegate call opened another pane than the delegates pane').toBe(id)
})

// --- 3. the transcript view ---

test('M3a-3: choosing a row shows the tail of that delegate\'s own transcript, and a control goes back to the list', async ($, on) => {
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
  const pane = await mountPane($, world)

  await openRow(pane, world, alpha.id, ids)
  let text = textOf(await drawn(pane))
  expect(hasWord(text, 'alpha-user-final'), `the view of ${alpha.id} lacks its transcript's last user line: ${clip(text)}`).toBe(true)
  expect(hasWord(text, 'alpha-reply-final'), `the view of ${alpha.id} lacks its transcript's last assistant line: ${clip(text)}`).toBe(true)
  for (const word of ['beta-user-final', 'beta-reply-final', 'slugdecoy-reply-final', 'hostdecoy-reply-final']) {
    expect(hasWord(text, word), `the view of ${alpha.id} shows ${word}, which is not its transcript's`).toBe(false)
  }

  const back = await backToList(pane, world, ids, ['alpha-user-final', 'alpha-reply-final'])
  expect(back, `no control in the view of ${alpha.id} goes back to the list`).toBe(true)

  await openRow(pane, world, beta.id, ids)
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
  const ids = [first.id, second.id, finished.id]
  const monitorArgs = () => world.mcpCalls.filter(c => c.tool === 'monitor').map(c => c.args)
  const answered = () => world.mcpCalls.filter(c => c.tool === 'monitor' && c.answered).map(c => c.args)
  const pane = await mountPane($, world)

  await pressStop(pane, world, first.id, ids)
  expect(answered(), `stopping ${first.id} did not call clauth's monitor exactly once, for it alone`).toEqual([
    { job_ids: [first.id], cancel: true },
  ])
  for (const args of monitorArgs()) {
    expect(args, `a monitor call while stopping ${first.id} carried other arguments`).toEqual({ job_ids: [first.id], cancel: true })
  }

  if (rowFor(await drawn(pane), second.id, ids) === undefined) {
    const back = await backToList(pane, world, ids, [])
    expect(back, `no control goes back to the list after stopping ${first.id}`).toBe(true)
  }
  await pressStop(pane, world, second.id, ids)
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
  const ids = [direct.id, viaDone.id]
  for (const job of [direct, viaDone]) await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  let pane = await mountPane($, world)
  for (const job of [direct, viaDone]) expectRow(await drawn(pane), job, ids, world.clock.now())

  const doneAt = world.clock.now()
  world.job({ ...viaDone, state: 'done', doneAt })
  await world.clock.advance(POLL_MS)
  world.deliver(direct.id, direct.profile)
  world.deliver(viaDone.id, viaDone.profile)
  await world.clock.advance(POLL_MS)
  const expectFinished = async (when: string) => {
    const tree = await drawn(pane)
    for (const job of [direct, viaDone]) {
      const row = rowFor(tree, job.id, ids)
      expect(row !== undefined, `${when}, the pane no longer lists ${job.id}`).toBe(true)
      const text = textOf(row)
      expect(hasWord(text, job.profile), `${when}, the row for ${job.id} lost its account ${job.profile}: ${clip(text)}`).toBe(true)
      expect(FINISHED_WORD.test(text), `${when}, the row for ${job.id} does not say done: ${clip(text)}`).toBe(true)
      expect(RUNNING_WORD.test(text), `${when}, the row for ${job.id} still says running: ${clip(text)}`).toBe(false)
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

test('M3a-7: with no option set and no command run, one delegate call opens the pane, lists the job and notices its finish', async ($, on) => {
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzf7aaa-0', 'D1', 30_000)
  await delegate($, world, job)
  await world.clock.settle()
  expect(world.opens.length, 'with no option set, a delegate call did not open the pane').toBe(1)
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  expectRow(await drawn(pane), job, [job.id], world.clock.now())
  const before = noticesNaming(world, job.id)
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
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
  expect(world.opens.length, 'a completed clauth delegate call did not open the pane').toBe(1)
  const id = world.opens[0]!.id

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

// --- 9. reopening the pane: a control in the hint line under the prompt ---

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

test('M3a-9b: once the session has a delegate the hint line keeps the engine\'s line and adds a control that opens the delegates pane', { plugins: [PANE_CLOSER] }, async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzf9aaa-0', 'D1', 1_000)
  await delegate($, world, job)
  await world.clock.advance(POLL_MS)
  const id = world.opens[0]!.id
  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(id), 'the pane stayed open after it was closed').toBe(false)
  // A finished delegate still counts: the session has had one.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)

  const hint = await mountHint($)
  await hint.redraw()
  const tree = await hint.drawn()
  expect(textOf(tree).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(textOf(tree))}`).toBe(true)
  const controls = buttonsIn(tree)
  expect(controls.length, `the hint line holds no single control for the delegates pane: ${clip(textOf(tree))}`).toBe(1)
  const opensBefore = world.opens.length
  await hint.press({ key: keyOf(controls[0]!) })
  await world.clock.settle()
  expect(world.opens.length - opensBefore, 'pressing the hint line\'s control opened no pane').toBe(1)
  expect(world.opens.at(-1)!.id, 'the hint line\'s control opened another pane than the delegates pane').toBe(id)
  await hint.unmount()
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
  const pane = await mountPane($, world)
  expectRow(await drawn(pane), job, [job.id], world.clock.now())

  // clauth's claim renames the record away before the ledger lands: the scan
  // in that window must not settle the delivery as ended.
  world.remove(`${JOBS}/${job.id}.json`)
  await world.clock.advance(2 * TICK_MS)
  const midway = textOf(await drawn(pane))
  expect(RUNNING_WORD.test(midway), `the row for ${job.id} settled while its delivery ledger was still being written: ${clip(midway)}`).toBe(true)
  expect(noticesNaming(world, job.id), `a notice named ${job.id} before its delivery ledger landed`).toBe(0)

  world.put(`${JOBS}/${job.id}.json.delivered`, ledgerFile(job.id, job.profile, world.clock.now()))
  await world.clock.advance(TICK_MS)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the running ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const after = textOf(await drawn(pane))
  expect(hasWord(after, late.id), `the pane lists ${late.id} although nothing live should scan: ${clip(after)}`).toBe(false)
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
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = textOf(row!)
  expect(/\bstopped\b/.test(text), `the stop at the session's exit reads done, not stopped: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the stopped ${job.id} still reads running: ${clip(text)}`).toBe(false)
  expect(
    buttonsIn(row!).some(b => STOP_WORD.test(textOf(b))),
    `the stopped ${job.id} still offers a stop control: ${clip(text)}`,
  ).toBe(false)
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
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the swept ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  const ids = [job.id, keeper.id]
  const orphaned = textOf(rowFor(await drawn(pane), job.id, ids) ?? '')
  expect(/\borphaned\b/.test(orphaned), `the run whose owner is gone does not read orphaned: ${clip(orphaned)}`).toBe(true)
  expect(noticesNaming(world, job.id), `the orphaned ${job.id} raised no notice`).toBe(1)

  // The startup sweep converts the corpse into its stopped tombstone.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() - 1_000, stopped: true, crashed: true })
  await world.clock.advance(TICK_MS)
  const row = rowFor(await drawn(pane), job.id, ids)
  expect(row !== undefined, `the pane lost ${job.id} when its record became the stopped tombstone`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  expect(rowFor(await drawn(pane), job.id, [job.id]) !== undefined, `the pane lost the running ${job.id}`).toBe(true)

  // clauth collects the stopped record between two reads: the record goes,
  // its ledger stays with the stop marker the claim judged.
  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, stoppedLedgerFile(job.id, job.profile, world.clock.now()))
  await world.clock.advance(TICK_MS)

  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = textOf(row!)
  expect(/\bstopped\b/.test(text), `the stop collected into its ledger reads done, not stopped: ${clip(text)}`).toBe(true)
  expect(/\bdone\b/.test(text), `the stopped ${job.id} still reads done: ${clip(text)}`).toBe(false)
  expect(RUNNING_WORD.test(text), `the stopped ${job.id} still reads running: ${clip(text)}`).toBe(false)
  expect(
    buttonsIn(row!).some(b => STOP_WORD.test(textOf(b))),
    `the stopped ${job.id} still offers a stop control: ${clip(text)}`,
  ).toBe(false)
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
  const pane = await mountPane($, world)

  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, ledgerFile(job.id, job.profile, world.clock.now()))
  await world.clock.advance(TICK_MS)

  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the collected ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  const ids = [job.id, keeper.id]
  const orphaned = textOf(rowFor(await drawn(pane), job.id, ids) ?? '')
  expect(/\borphaned\b/.test(orphaned), `fixture control: not orphaned: ${clip(orphaned)}`).toBe(true)

  // One monitor call by id: the reader sweep converts the corpse and the claim
  // collects it, so the pane never reads the tombstone, only the ledger.
  world.remove(`${JOBS}/${job.id}.json`)
  world.put(`${JOBS}/${job.id}.json.delivered`, stoppedLedgerFile(job.id, job.profile, world.clock.now()))
  await world.clock.advance(TICK_MS)
  await world.clock.advance(TICK_MS)
  const text = textOf(rowFor(await drawn(pane), job.id, ids) ?? '')
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
  const pane = await mountPane($, world)
  const ids = [keeper.id, blocking.id]
  const shownBlocking = textOf(rowFor(await drawn(pane), blocking.id, ids) ?? '')
  expect(/\bblocking\b/.test(shownBlocking), `the pane draws no blocking row for ${blocking.id}: ${clip(shownBlocking)}`).toBe(true)

  // The stop lands while the call is in flight; the call's own settle then
  // carries the cancel.
  armed = true
  world.remove(`${JOBS}/${blocking.id}.live.json`)
  const settled = $.tool.call({ tool: DELEGATE, profile: blocking.profile, prompt: 'block', background: false })
  await pressStop(pane, world, blocking.id, ids)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(blocking.id)).length,
    `stopping the blocking ${blocking.id} did not ask clauth's monitor`,
  ).toBe(1)
  await settled
  await world.clock.settle()
  await world.clock.advance(POLL_MS)

  const row = rowFor(await drawn(pane), blocking.id, ids)
  expect(row !== undefined, `the pane lost the blocking ${blocking.id}`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  const ids = [before.id, fresh.id]
  const tree = await drawn(pane)
  expect(rowFor(tree, before.id, ids) !== undefined, `the /clear lost the running ${before.id}: ${clip(textOf(tree))}`).toBe(true)
  expect(rowFor(tree, fresh.id, ids) !== undefined, `the /clear's own ${fresh.id} is not listed`).toBe(true)

  await pressStop(pane, world, before.id, ids)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(before.id)).length,
    `the pre-clear run ${before.id} has no working stop after the /clear`,
  ).toBe(1)

  world.job({ ...before, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
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
  expectRow(await drawn(pane), job, [job.id], world.clock.now())
  await pane.unmount()
})

test('M3a-E2: the hint line reads the store, so a job written without a delegate call gets its control', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  world.job(ownJob(world.clock.now(), 'd-mvzfe002-0', 'D1', 60_000))
  const hint = await mountHint($)
  await world.clock.settle()
  await hint.redraw()
  const tree = await hint.drawn()
  expect(textOf(tree).includes(ENGINE_HINT), `the engine's hint line is gone: ${clip(textOf(tree))}`).toBe(true)
  expect(buttonsIn(tree).length, `the hint line shows no control for a job written without a delegate call: ${clip(textOf(tree))}`).toBe(1)
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
  const pane = await mountPane($, world)
  await openRow(pane, world, job.id, [job.id])
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
  await pressStop(pane, world, job.id, [job.id])
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
  const pane = await mountPane($, world)
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
  expect(noticesNaming(world, job.id), `the finished ${job.id} was not noticed`).toBe(1)

  const late = ownJob(world.clock.now(), 'd-mvzfh002-1', 'D2', 1_000)
  world.job(late)
  await world.clock.advance(3 * POLL_MS)
  const stopped = textOf(await drawn(pane))
  expect(hasWord(stopped, late.id), `the pane lists ${late.id}, so the ticks never stopped: ${clip(stopped)}`).toBe(false)
  expect(noticesNaming(world, late.id), `a notice named ${late.id}, so the ticks never stopped`).toBe(0)

  // The control: a new delegate call starts the ticks again, and the same
  // record is then read.
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfh003-2', 'D3', 1_000))
  await world.clock.advance(POLL_MS)
  const restarted = textOf(await drawn(pane))
  expect(hasWord(restarted, late.id), `a delegate call no longer restarts the store's ticks, so ${late.id} stays unread: ${clip(restarted)}`).toBe(true)
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
  const pane = await mountPane($, world)
  await openRow(pane, world, job.id, [job.id])
  const shown = textOf(await drawn(pane))
  expect(hasWord(shown, 'long-tail-marker'), `the view misses the line inside the last 512 KiB: ${clip(shown)}`).toBe(true)
  expect(hasWord(shown, 'long-reply-final'), `the view misses the transcript's last reply: ${clip(shown)}`).toBe(true)
  await pane.unmount()
})

test('M3a-I: closing the pane drops the transcript view, so a reopen lands on the list and no read runs', { plugins: [PANE_CLOSER] }, async ($, on) => {
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
  await world.clock.advance(POLL_MS)
  const pane = await mountPane($, world)
  await openRow(pane, world, job.id, [job.id])
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
  await world.clock.advance(POLL_MS)
  expect(tails.length, `${tails.length - readsAfterClose} transcript reads ran with the pane closed`).toBe(readsAfterClose)
  await pane.unmount()

  await $.command.run({ command: 'delegates' })
  await world.clock.settle()
  const reopened = await mountPane($, world)
  const tree = await drawn(reopened)
  expect(rowFor(tree, job.id, [job.id]) !== undefined, `the reopened pane does not list ${job.id}`).toBe(true)
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
  const pane = await mountPane($, world)
  const ids = [job.id]
  expectRow(await drawn(pane), job, ids, world.clock.now())

  // The engine's /clear: a new session id, no `session.start`, and the rows
  // the ended conversation's session held are gone.
  sid = 'sid-after-clear'
  await $.session.end({ reason: 'clear', sessionId: OWN, resume: { id: OWN } })
  stateDrop.drop()

  // The record has not moved; the ticks keep reading for it.
  await world.clock.advance(TICK_MS)
  const tree = await drawn(pane)
  expect(
    rowFor(tree, job.id, ids) !== undefined,
    `the /clear lost the running ${job.id} whose record was read before it: ${clip(textOf(tree))}`,
  ).toBe(true)
  expect(
    RUNNING_WORD.test(textOf(rowFor(tree, job.id, ids) ?? '')),
    `the re-adopted ${job.id} does not read running`,
  ).toBe(true)

  await pressStop(pane, world, job.id, ids)
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(job.id)).length,
    `the re-adopted ${job.id} has no working stop`,
  ).toBe(1)

  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(POLL_MS)
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
  const tree = await drawn(pane)
  expect(
    rowFor(tree, old.id, [old.id]) === undefined,
    `the next session adopted ${old.id} from the conversation the /clear ended: ${clip(textOf(tree))}`,
  ).toBe(true)
  await pane.unmount()
})

test('M3a-L1: the hint line asks the store once a second while the ticks are stopped, not on every draw', async ($, on) => {
  const lists: string[] = []
  on('fs.list', { path: JOBS }, ($, e, next) => {
    lists.push(e.path)
    return next(e)
  })
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  const hint = await mountHint($)
  await world.clock.settle()
  const afterFirst = lists.length

  for (let i = 0; i < 3; i += 1) {
    await world.clock.advance(300)
    await hint.redraw()
    await world.clock.settle()
  }
  expect(
    lists.length - afterFirst,
    `the hint line listed the store on each of three draws inside a second: ${lists.length - afterFirst} listings`,
  ).toBe(0)

  // A draw a second later still finds a job minted outside a delegate call.
  await world.clock.advance(1000)
  world.job(ownJob(world.clock.now(), 'd-mvzfl001-0', 'D1', 60_000))
  await hint.redraw()
  await world.clock.settle()
  await hint.redraw()
  const tree = await hint.drawn()
  expect(buttonsIn(tree).length, `the throttled hint line never read the store: ${clip(textOf(tree))}`).toBe(1)
  await hint.unmount()
})

test('M3a-L2: while the ticks run the hint line adds no store read of its own', async ($, on) => {
  const lists: string[] = []
  on('fs.list', { path: JOBS }, ($, e, next) => {
    lists.push(e.path)
    return next(e)
  })
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfl002-0', 'D1', 60_000))
  await world.clock.advance(POLL_MS)
  const hint = await mountHint($)
  await world.clock.settle()
  await hint.redraw()
  await world.clock.settle()
  const before = lists.length
  for (let i = 0; i < 3; i += 1) {
    await world.clock.advance(1000)
    await hint.redraw()
    await world.clock.settle()
  }
  // Three seconds hold one tick's own read; a draw adds none.
  expect(
    lists.length - before,
    `the hint line read the store ${lists.length - before} times over three draws while the ticks ran`,
  ).toBe(1)
  await hint.unmount()
})

test('M3a-M: a ps that rejects the owner probe\'s form never reads a live run as orphaned', async ($, on) => {
  on('process.run', { argv: [/^ps$/] }, () => processResult(1, '', 'ps: unknown user-defined format specifier "stat"'))
  const world = installWorld(on)
  await startSession($)
  const job = ownJob(world.clock.now(), 'd-mvzfm001-0', 'D1', 300_000)
  await delegate($, world, job)
  await world.clock.advance(2 * POLL_MS)
  const pane = await mountPane($, world)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the running ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  const ids = [keeper.id, blocking.id]
  expect(
    /\bblocking\b/.test(textOf(rowFor(await drawn(pane), blocking.id, ids) ?? '')),
    `the pane draws no blocking row for ${blocking.id}`,
  ).toBe(true)

  // The call settles cancelled before its own run spawned: the prose names no
  // session, so no row in the pane is its.
  armed = true
  world.remove(`${JOBS}/${blocking.id}.live.json`)
  await $.tool.call({ tool: DELEGATE, profile: 'D1', prompt: 'block', background: false })
  await world.clock.settle()
  await world.clock.advance(3 * TICK_MS)

  const row = rowFor(await drawn(pane), blocking.id, ids)
  expect(row !== undefined, `the pane lost the blocking ${blocking.id}`).toBe(true)
  const text = textOf(row!)
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
  const pane = await mountPane($, world)
  expectRow(await drawn(pane), job, [job.id], world.clock.now())

  // No delegate call ever ran, so the ticks must come from the read itself.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(TICK_MS)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the adopted ${job.id}`).toBe(true)
  expect(FINISHED_WORD.test(textOf(row!)), `the adopted ${job.id} was never read again: ${clip(textOf(row!))}`).toBe(true)
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
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the silent ${job.id}`).toBe(true)
  const text = textOf(row!)
  expect(/\borphaned\b/.test(text), `the ownerless record silent past the TTL does not read orphaned: ${clip(text)}`).toBe(true)
  expect(RUNNING_WORD.test(text), `the ownerless corpse still reads running: ${clip(text)}`).toBe(false)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(/\borphaned\b/.test(notices[0]!.text), `the notice for ${job.id} does not say orphaned: ${clip(notices[0]!.text)}`).toBe(true)
  await pane.unmount()
})

// --- 12. the open as the run starts, the layout gate, the `/clear` race ---

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

test('M3a-Q1: a blocking delegate call held open opens the pane while it runs', async ($, on) => {
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
  const job = ownJob(world.clock.now(), 'd-mvzfq001-0', 'D1', 10_000, { sessionId: 'sess-held' })
  world.put(`${JOBS}/${job.id}.live.json`, jobFile(job))
  let returned = false
  const call = $.tool.call({ tool: DELEGATE, profile: job.profile, prompt: 'block', background: false }).then(v => {
    returned = true
    return v
  })
  await world.clock.advance(POLL_MS)
  expect(returned, 'the blocking call had already settled, so the run was never shown while it ran').toBe(false)
  expect(world.opens.length, 'a blocking call\'s live run opened no pane while the call still ran').toBe(1)
  expect(world.open.has(world.opens[0]!.id), 'the pane the live run opened is not open').toBe(true)
  release()
  await call
  await world.clock.settle()
})

test('M3a-Q2: a pane the person closed during the call is not reopened when it settles', { plugins: [PANE_CLOSER] }, async ($, on) => {
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
  const job = ownJob(world.clock.now(), 'd-mvzfq002-0', 'D1', 10_000, { sessionId: 'sess-held-2' })
  world.put(`${JOBS}/${job.id}.live.json`, jobFile(job))
  const call = $.tool.call({ tool: DELEGATE, profile: job.profile, prompt: 'block', background: false })
  await world.clock.advance(POLL_MS)
  expect(world.opens.length, 'the live run opened no pane to close').toBe(1)
  const id = world.opens[0]!.id
  await $.turn.start({ text: 'close the panes', turnId: 't-close' })
  await $.turn.complete({ reason: 'answer', answer: '', durationMs: 1, isAborted: false, turnId: 't-close' })
  expect(world.open.has(id), 'the pane stayed open after it was closed').toBe(false)
  const openedBefore = world.opens.length
  release()
  await call
  await world.clock.settle()
  await world.clock.advance(TICK_MS)
  expect(
    world.opens.length - openedBefore,
    'the settling call reopened the pane the person closed during it',
  ).toBe(0)
})

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

test('M3a-R2: a fullscreen surface still opens the pane unasked for a delegate call', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  const hint = await mountHintOn($, true)
  await hint.redraw()
  await world.clock.settle()
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfr002-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a fullscreen surface opened no pane for a delegate call').toBe(1)
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
  const ids = [fin.id, live.id]
  expect(
    rowFor(tree, fin.id, ids) === undefined,
    `the scan that straddled the /clear wrote the ended conversation's finished ${fin.id} into the new one: ${clip(textOf(tree))}`,
  ).toBe(true)
  expect(
    rowFor(tree, live.id, ids) !== undefined,
    `the /clear lost the still live ${live.id}: ${clip(textOf(tree))}`,
  ).toBe(true)
  await pane.unmount()
})

// --- 13. a stale finished record, the cancel reply, the hint's row ---

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
    rowFor(await drawn(pane), old.id, [old.id]) !== undefined,
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
  const pane = await mountPane($, world)
  await pressStop(pane, world, job.id, [job.id])
  // clauth's collect lands with no cancelled envelope: the record goes and the
  // ledger stays, as the live run's stopped job left it.
  world.deliver(job.id, job.profile)
  await world.clock.advance(TICK_MS)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = textOf(row!)
  expect(/\bcancelled\b/.test(text), `the stopped ${job.id} reads done, not cancelled: ${clip(text)}`).toBe(true)
  const notices = world.notices.filter(n => hasWord(n.text, job.id))
  expect(notices.length, `${job.id} was noticed ${notices.length} times, not once`).toBe(1)
  expect(
    /\bcancelled\b/.test(notices[0]!.text),
    `the notice for ${job.id} does not say cancelled: ${clip(notices[0]!.text)}`,
  ).toBe(true)
  await pane.unmount()
})

test('M3a-V: the control sits on its own row under the engine\'s hint, the engine\'s node untouched', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzfv001-0', 'D1', 1_000))
  await world.clock.advance(POLL_MS)
  const hint = await mountHint($)
  await hint.redraw()
  const tree = await hint.drawn()
  expect(isDrawn(tree) && tree.type === 'Box', `the hint line is no Box: ${clip(textOf(tree))}`).toBe(true)
  const root = tree as Drawn
  expect(
    root.props?.flexDirection,
    `the hint line is not laid out as the engine's row above the control's: ${clip(JSON.stringify(root))}`,
  ).toBe('column')
  const kids = (root.children ?? []).filter(isDrawn)
  const engine = kids.find(el => textOf(el).includes(ENGINE_HINT))
  expect(engine !== undefined, 'the engine\'s own hint node left the line').toBe(true)
  expect(
    Number(engine!.props?.flexShrink ?? 0),
    `the engine's node was rewritten to give the control width: ${clip(JSON.stringify(root))}`,
  ).toBe(0)
  const control = buttonsIn(root).find(button => keyOf(button) === 'delegates')
  expect(control !== undefined, 'the hint line holds no control').toBe(true)
  const controlRow = kids.find(el => el !== engine && buttonsIn(el).includes(control!))
  expect(controlRow !== undefined, 'the control is not on a row of its own under the engine\'s node').toBe(true)
  expect(
    buttonsIn(engine!).length,
    `the engine's own node holds the control: ${clip(JSON.stringify(root))}`,
  ).toBe(0)
  await hint.unmount()
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
  const pane = await mountPane($, world)

  refusing = true
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await world.clock.advance(TICK_MS)
  expect(noticesNaming(world, job.id), 'a notice went out before the rows were stored').toBe(0)

  refusing = false
  await world.clock.advance(TICK_MS)
  expect(
    noticesNaming(world, job.id),
    `the scan marked the record read before its rows landed, so the finished ${job.id} was never noticed`,
  ).toBe(1)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined && FINISHED_WORD.test(textOf(row)), `the row for ${job.id} never settled done`).toBe(true)
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
  const pane = await mountPane($, world)
  await openRow(pane, world, job.id, [job.id])
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

// --- 15. the layout per surface, the session epoch, the stop's replies ---

test('M3a-Y1: a surface that has not reported its layout keeps the pane closed', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  world.setSurfaces(['terminal', 'mobile'])
  const term = await mountHintOn($, true)
  await term.redraw()
  await world.clock.settle()
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzy0001-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a surface that has not said its layout still got an unasked open').toBe(0)
  await term.unmount()
})

test('M3a-Y2: a main-screen terminal keeps the pane closed though another surface docks it', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  world.setSurfaces(['terminal', 'desktop'])
  const term = await mountHintOn($, false)
  await term.redraw()
  const desktop = await $.ui.mount({
    plugin: 'clauth',
    surface: 'desktop',
    component: 'Pane',
    requestId: 'clauth-delegates',
    props: { ...PANE_PROPS, title: 'clauth delegates' },
    viewport: { columns: 200, rows: 50, isFullscreen: true },
  })
  await desktop.redraw()
  await world.clock.settle()
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzy0002-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a surface drawn last decided the main-screen terminal\'s unasked open').toBe(0)
  await desktop.unmount()
  await term.unmount()
})

test('M3a-Y3: every attached surface docking the pane still opens it once', async ($, on) => {
  installEngineHint(on)
  const world = installWorld(on)
  await startSession($)
  world.setSurfaces(['terminal', 'mobile'])
  const term = await mountHintOn($, true)
  await term.redraw()
  const phone = await $.ui.mount({
    plugin: 'clauth',
    surface: 'mobile',
    component: 'Pane',
    requestId: 'clauth-delegates',
    props: { ...PANE_PROPS, title: 'clauth delegates' },
    viewport: { columns: 80, rows: 40, isFullscreen: true },
  })
  await phone.redraw()
  await world.clock.settle()
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzy0003-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a session whose every surface docks the pane got no unasked open').toBe(1)
  await phone.unmount()
  await term.unmount()
})

test('M3a-Y4: a session that draws on no surface gets no unasked open', async ($, on) => {
  const world = installWorld(on)
  await $.session.start({ cwd: HOST_CWD, surface: null, isInteractive: false })
  world.setSurfaces([])
  await delegate($, world, ownJob(world.clock.now(), 'd-mvzy0004-0', 'D1', 1_000))
  await world.clock.settle()
  expect(world.opens.length, 'a session with no surface got an unasked open').toBe(0)
})

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
  const tree = await drawn(pane)
  expect(
    rowFor(tree, job.id, [job.id]) === undefined,
    `the scan that straddled the /resume listed the ended conversation's ${job.id} in the resumed one: ${clip(textOf(tree))}`,
  ).toBe(true)
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
  const ids = [keeper.id, blocking.id]
  await pressStop(pane, world, blocking.id, ids)
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
  const pane = await mountPane($, world)

  // The run finished; no scan has read its done record yet, so its row still
  // offers the stop.
  world.job({ ...job, state: 'done', doneAt: world.clock.now() })
  await pressStop(pane, world, job.id, [job.id])
  expect(
    world.mcpCalls.filter(c => c.tool === 'monitor' && (c.args.job_ids as string[]).includes(job.id)).length,
    `the stop collected the finished ${job.id}, so its result went to the mod instead of the model`,
  ).toBe(0)

  await world.clock.advance(TICK_MS)
  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the finished ${job.id}`).toBe(true)
  expect(FINISHED_WORD.test(textOf(row!)), `the finished ${job.id} does not read done: ${clip(textOf(row!))}`).toBe(true)
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
  const pane = await mountPane($, world)
  await pressStop(pane, world, job.id, [job.id])
  world.deliver(job.id, job.profile)
  await world.clock.advance(TICK_MS)

  const row = rowFor(await drawn(pane), job.id, [job.id])
  expect(row !== undefined, `the pane lost the stopped ${job.id}`).toBe(true)
  const text = textOf(row!)
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
  const world = installWorld(on)
  const old = ownJob(T0 - 86_000_000, 'd-mvzy0012-0', 'D1', 600_000)
  world.job({ ...old, state: 'done', doneAt: T0 - 85_000_000 })

  const starting = $.session.start({ cwd: HOST_CWD, surface: 'terminal', isInteractive: true })
  await inRegister
  // The surface draws once while the start is still in flight, and the draw
  // asks for a store read on the next clock tick.
  const hint = await mountHint($)
  await hint.redraw()
  await world.clock.settle()
  expect(
    noticesNaming(world, old.id),
    `a store read during session.start toasted ${old.id}, which ended before this process started`,
  ).toBe(0)

  release()
  await starting
  await world.clock.settle()
  await hint.unmount()
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
    const ids = [fin.id, live.id]
    expect(
      rowFor(tree, live.id, ids) !== undefined,
      `the /clear lost the live ${live.id} the read inside its end carried away: ${clip(textOf(tree))}`,
    ).toBe(true)
    expect(
      rowFor(tree, fin.id, ids) === undefined,
      `a read inside the /clear listed the ended conversation's finished ${fin.id}: ${clip(textOf(tree))}`,
    ).toBe(true)
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

    // The /resume's end stops the ticks and awaits its post; the redrawn hint
    // line asks for a read meanwhile.
    world.job({ ...job, tail: 'reading the build config again' })
    endHold.arm()
    const ending = $.session.end({ reason: 'resume', sessionId: OWN, resume: { id: OWN } })
    await endHold.inEnd
    let ticking: Promise<unknown> = Promise.resolve()
    if (scanInWindow) {
      const hint = await mountHintOn($, true)
      await hint.redraw()
      await hint.unmount()
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
    const tree = await drawn(pane)
    expect(
      rowFor(tree, job.id, [job.id]) === undefined,
      `a read inside the /resume listed the ended conversation's ${job.id} in the resumed one: ${clip(textOf(tree))}`,
    ).toBe(true)
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
    const ids = [fin.id, live.id]
    expect(
      rowFor(tree, fin.id, ids) === undefined,
      `a read inside the /clear wrote the ended conversation's finished ${fin.id} into the new one: ${clip(textOf(tree))}`,
    ).toBe(true)
    expect(
      rowFor(tree, live.id, ids) !== undefined,
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
    const ids = [keeper.id, blocking.id]
    expect(
      /\bblocking\b/.test(textOf(rowFor(await drawn(pane), blocking.id, ids) ?? '')),
      `the pane draws no blocking row for ${blocking.id}`,
    ).toBe(true)

    // Esc hands the blocking run off to a job file, and the run lands before
    // the next scan reads that file: the row still offers its stop.
    if (handedOff) {
      world.remove(`${JOBS}/${blocking.id}.live.json`)
      world.job({ ...blocking, state: 'done', doneAt: world.clock.now() })
    }
    await pressStop(pane, world, blocking.id, ids)
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
    rowFor(tree, job.id, [job.id]) !== undefined,
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
  const tree = await drawn(pane)
  expect(
    rowFor(tree, job.id, [job.id]) === undefined,
    `the write the /resume landed under put the ended conversation's ${job.id} into the resumed one: ${clip(textOf(tree))}`,
  ).toBe(true)
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
    const tree = await drawn(pane)
    expect(
      rowFor(tree, job.id, [job.id]) === undefined,
      `the /resume listed the ${left} conversation's carried ${job.id} in the resumed one: ${clip(textOf(tree))}`,
    ).toBe(true)
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
    const tree = await drawn(pane)
    expect(
      rowFor(tree, job.id, [job.id]) === undefined,
      `the /resume adopted the ${left} conversation's running ${job.id}: ${clip(textOf(tree))}`,
    ).toBe(true)
    world.job({ ...job, state: 'done', doneAt: world.clock.now() })
    await world.clock.advance(3 * TICK_MS)
    await world.clock.settle()
    const after = await drawn(pane)
    expect(
      rowFor(after, job.id, [job.id]) === undefined,
      `the /resume listed the ${left} conversation's finished ${job.id}: ${clip(textOf(after))}`,
    ).toBe(true)
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
        rowFor(tree, bJob.id, [bJob.id]) !== undefined,
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
  const ids = [bJob.id, own.id]
  expect(
    rowFor(tree, bJob.id, ids) !== undefined,
    `the /resume left out the moved ${bJob.id} of the conversation it entered: ${clip(textOf(tree))}`,
  ).toBe(true)
  expect(
    rowFor(tree, own.id, ids) === undefined,
    `the /resume listed the left conversation's finished ${own.id}: ${clip(textOf(tree))}`,
  ).toBe(true)
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
  const ids = [fin.id, live.id]
  expect(readsInWindow, 'a store read started inside the /clear\'s end and listed the store').toBe(0)
  expect(
    rowFor(tree, live.id, ids) !== undefined,
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
  const row = rowFor(tree, job.id, [job.id])
  expect(
    row !== undefined,
    `the failed write lost the carried ${job.id} before its retry: ${clip(textOf(tree))}`,
  ).toBe(true)
  expect(
    row !== undefined && FINISHED_WORD.test(textOf(row)),
    `the carried ${job.id} never settled done on the retry: ${clip(textOf(tree))}`,
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

test('M3a-AC1: an unheld ask on a delegate call closes once by the desk and the pane still opens', async ($, on) => {
  const events = recordBridgeEvents(on)
  on('tool.check', ($, e, next) => (e.tool === DELEGATE ? { decision: 'ask' as const } : next(e)))
  const dialogs = deskDialogs(on)
  // clauth writes the record only once the permitted call runs, so no tick
  // while the dialog is up can find the run: the open this test sees is the
  // settle-time one.
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
  expect(world.opens.length, 'the delegate call did not auto-open the pane once').toBe(1)
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
