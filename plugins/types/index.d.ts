// The clauth mod's `$.state` contract: values the desk row's drawing reads,
// the event sequence counter and the phone-prompt queue, each held by the host
// so a hot reload of the module keeps them.
export type ClauthHeldRow = {
  /** The ask id the held check was posted under. */
  ask: string
  /** Which row line 2 shows: a permission ask's Allow/Deny, or an
   *  AskUserQuestion's answer-here. */
  kind: 'decision' | 'answers'
  /** The tool name, shown as line 1's `<Tool>`. */
  tool: string
  /** What the tool acts on (Bash's command, a path, a question, or one-line
   *  JSON), cut to the band's width at draw time. */
  desc: string
}

export type ClauthQueuedPrompt = {
  /** The phone prompt's text. */
  text: string
  /** The daemon's id for it; absent from an older daemon. */
  id?: string
  /** Refused tries so far. */
  tries: number
  /** Present only while this prompt's submit is out: a fresh load finds it
   *  and reports its fate unknown rather than sending it twice. */
  submitting?: true
}

/** A delegate's state word: `running` and `blocking` are live (a background
 *  job, a blocking run whose caller holds the join); every other word is
 *  final. `stopped` is a run its Claude Code session's exit stopped; `ended`
 *  is a live run whose record went with no result left. */
export type ClauthDelegateWord =
  | 'running'
  | 'blocking'
  | 'done'
  | 'cancelled'
  | 'stopped'
  | 'orphaned'
  | 'ended'

/** One delegate of this session, as its job record last read. */
export type ClauthDelegateRow = {
  /** The job id. */
  id: string
  /** The account it runs on. */
  profile: string
  word: ClauthDelegateWord
  /** Epoch ms the run started; 0 when the record named none. */
  startedAt: number
  /** Epoch ms the run ended; 0 while live or when unknown. */
  endedAt: number
  /** Epoch ms of the run's last output; 0 when the record names none. */
  lastOutputAt: number
  /** The record's tail of assistant text, as the record has it. Never drawn
   *  (a cut tail starts mid-word); stored so a record whose only change is new
   *  text still differs from the stored rows and rewrites them. */
  tail: string
  /** The delegate's own transcript file, null when the record lacks its
   *  session id or working directory. */
  transcript: string | null
  /** Whether its leaving the live words was told to the person. */
  isNoticed: boolean
}

/** One entry of a delegate's transcript as the pane draws it. Each carries
 *  its anchor key, which the pane's scroll window holds its place by across
 *  reads: a call's is its own id, the rest their entry's uuid with the
 *  block's index. */
export type ClauthTranscriptItem =
  /** A tool call as Claude Code draws it (`Bash(cargo test)`), with its
   *  result's summary once it came back: its first lines, and how many more
   *  it holds. */
  | { key: string; kind: 'call'; call: string; result: string[] | null; more: number; isError: boolean }
  /** One message of the delegate's own, its lines as written (markdown). */
  | { key: string; kind: 'said'; lines: string[] }
  /** A prompt the delegate was given after its first. */
  | { key: string; kind: 'asked'; text: string }
  /** Why there is nothing to draw: no transcript, or none written yet. */
  | { key: string; kind: 'note'; text: string }

/** The delegate the pane's detail shows: its name, its model, its tool calls
 *  and the end of its transcript. */
export type ClauthDelegateView = {
  id: string
  /** The first line of the delegate's own prompt, empty when the
   *  transcript's head names none. */
  name: string
  /** The transcript entry holding that prompt, which the header shows in its
   *  place; empty while unknown. */
  promptId: string
  /** The model of its newest message, empty before it wrote one. */
  model: string
  /** How many tool calls the whole transcript holds; null when uncounted. */
  toolCalls: number | null
  items: ClauthTranscriptItem[]
}

declare module 'claude-code' {
  interface PluginState {
    clauth: {
      /** The highest sequence number already sent on the bridge; the next
       *  event sends `seq + 1`, so a module reload never re-sends an old one. */
      seq: number
      /** The desk row shown above the prompt while a check is held, null when
       *  nothing is held. */
      heldRow: ClauthHeldRow | null
      /** Phone prompts waiting their turn, oldest first. */
      prompts: ClauthQueuedPrompt[]
      /** Every delegate this session spawned, finished ones kept after clauth
       *  drops their records, newest first. */
      delegates: Shaped<ClauthDelegateRow[]>
      /** The delegate the pane's detail shows, null until the pane opens on
       *  one and again once it closes. */
      delegateView: Shaped<ClauthDelegateView | null>
    }
  }
}
