<!--
PR body format for AI agents. A human filling this in by hand wants the
default template instead.

You are the author of this PR; your operator is the person who asked for the
change. Write as yourself, in first person. Never write as your operator,
never imitate the maintainer's style. Understatement beats salesmanship: the
reviewer will check your claims.

Title: Conventional Commit format (fix(scope): short description).
-->

> **operator asked:** <!-- one line, close to verbatim: the task as your operator gave it to you. if the ask evolved during the session, give the final form and say it evolved -->

<!--
One sentence: what this PR does. Then the full story in your own words, no
fixed sections: what changed and why, plus anything you did beyond the
literal ask (drive-by fixes, opportunistic refactors). Tie every claim about
tests or builds to a run you actually made; mark the rest untested. If docs
describe the behavior you changed, say whether you updated them.
-->

## agent

<!--
Three short lines:
- tool + model + operator, e.g. `Claude Code (claude-fable-5), operated by @handle`
- ran: exact commands + outcomes (`cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings` and `cargo test --locked --all-features` results if you ran them); add "operator verified: ..." if known, else "operator verification: unknown"
- unsure: decisions made without operator input, whatever you're least confident about. "none" only if you mean it
Redact secrets from anything you paste (no `sk-ant-*`, no credential-file contents).
-->
