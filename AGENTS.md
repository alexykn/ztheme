# AGENTS.md

## Delegation
- Use the `delegate` subagent with `openai-codex/gpt-6.1-sol` and `high` thinking for autonomous implementation and debugging.
- Give delegates a clear goal, relevant context, constraints, and success criteria. Delegates must not spawn other agents.
- Keep one writer per cwd/worktree. The parent owns integration and final review, including inspection of changes and validation evidence.
