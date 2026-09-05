# Contributing

Issues and PRs are welcome. File an issue through the [chooser](https://github.com/uwuclxdy/clauth/issues/new/choose), or open a PR against the default branch.

Found a security issue? Report it privately through the [security policy](https://github.com/uwuclxdy/clauth/security/policy). Don't file a public issue.

## Redact secrets

clauth handles live credentials. Before you paste any log, config, or output into an issue or PR, strip OAuth tokens, API keys, and the contents of `~/.claude/.credentials.json` or any `~/.clauth/profiles/*` file. An `sk-ant-*` string is a live credential, not an ID.

## Flag user-visible changes

Anything a user reads or presses can change without failing a build, so it reads as diff noise and slips past review. If your PR moves any of these, say so in the summary with why you changed it:

- wording a user reads: hints, labels, help text, error messages
- a key binding
- a default value a first run inherits

"reworded for X" or "changed the default because Y" is enough. A surface change with no reason behind it gets reverted even when the new value reads better.

## AI and agent contributions

AI-assisted work is welcome. The one requirement is transparency: a reviewer should be able to see who directed the change and which of its claims you actually verified.

### Human contributors

Any amount of AI help is fine. Use the default issue forms and PR template, ticking your AI-involvement level. You reviewed every line; the diff is yours.

### Autonomous agents

If you file on an operator's behalf, use the dedicated formats. They keep your authorship legible instead of disguised as a person's.

- Issues: the `(agent)` variant of your issue type in the [chooser](https://github.com/uwuclxdy/clauth/issues/new/choose) (e.g. Bug report (agent)).
- PRs: the agent format at `.github/PULL_REQUEST_TEMPLATE/agent.md`. Append `?template=agent.md` to the compare URL to load it.

Whichever you use:

- Write as yourself, first person. Don't imitate the maintainer's voice or pass the change off as a person's.
- Open with the operator's ask, close to verbatim. Follow it with your own account of the work.
- Say what you ran and what it returned. Keep that separate from what your operator verified. Flag anything you couldn't check.
- Prefer understatement. The reviewer will test your claims.
- Name your tool and model, plus the operator's handle. Redact secrets from anything you paste.
