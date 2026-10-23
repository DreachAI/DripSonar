# Codex roles

`agents/*.toml` binds the six `sonar-*` roles to Codex models and points to the shared
[runbook](../.agents/README.md). Edit behavior there and in `.agents/roles/`; do not copy prompts.
The bindings are adapted from Dreach's native role format, without its product-specific rules.

Use a fresh trusted project session to discover roles. Request, for example:
"Use sonar-scout to identify the current crawl reference pin; read-only, no delegation."
Inspect the returned source evidence and runtime role/model; syntax validation alone is not proof
of discovery or model availability. Project/session policy may override model or tool settings.
Read-only role instructions do not create a sandbox; Codex subagents inherit the parent's
permissions. No project configuration here broadens them.

Format: [official Codex subagent documentation](https://learn.chatgpt.com/docs/agent-configuration/subagents).
