# Claude Code roles

`agents/*.md` binds the same six `sonar-*` roles to Claude models and reads the shared
[runbook](../.agents/README.md). Edit behavior there and in `.agents/roles/`; do not copy prompts.
Root `CLAUDE.md` imports `AGENTS.md` so the project contract has one owner.

Use `/agents` in a fresh project session to inspect loaded definitions, then request a bounded
read-only `sonar-scout` question. Model availability and effective permissions depend on the client.
Tool allowlists narrow access; Bash can still write, so a read-only role's prose is not an OS sandbox.
Questions return to the parent, which owns any user clarification.

Validation note (2026-10-22): installed Claude Code 2.1.295 returned an empty result from
`claude plugin validate .claude/agents --strict --json`, even for a malformed temporary agent.
Do not use that result as a gate on this version. These bindings were instead parsed as YAML
and checked against the shared role paths; live discovery remains a separate check above.

Format: [official Claude Code subagent documentation](https://code.claude.com/docs/en/sub-agents).
