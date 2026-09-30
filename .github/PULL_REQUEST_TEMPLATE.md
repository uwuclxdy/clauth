<!--
PR title must start with a Conventional Commit type, e.g. fix(tui): ...
-->

## Summary

<!-- Describe the change and why. Link related issues; use a closing keyword (e.g. "closes #12") to auto-close. -->

This PR 

## Checklist

- [ ] Secrets redacted from any pasted output (no `sk-ant-*`, no `~/.claude`/`~/.clauth` contents)
- [ ] `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings` and `cargo test --locked --all-features` pass locally

## AI

<!-- Tick the closest. Any level of AI use is fine; this is just reviewer context. Authoring this PR as an autonomous agent? Use the agent format instead: `.github/PULL_REQUEST_TEMPLATE/agent.md` (append `?template=agent.md` to the compare URL). -->

- [ ] no AI
- [ ] AI-assisted, I reviewed everything
- [ ] AI did most of it under my direction
