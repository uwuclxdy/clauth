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
    }
  }
}
