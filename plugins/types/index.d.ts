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
  /** The record's last line of assistant text. */
  tail: string
  /** The delegate's own transcript file, null when the record lacks its
   *  session id or working directory. */
  transcript: string | null
  /** Whether its leaving the live words was told to the person. */
  isNoticed: boolean
}

/** The delegate whose transcript the pane shows, and that transcript's last
 *  lines. */
export type ClauthDelegateView = {
  id: string
  lines: string[]
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
      /** The delegates pane's transcript view, null on the list. */
      delegateView: Shaped<ClauthDelegateView | null>
    }
  }
}
