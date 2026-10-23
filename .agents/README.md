# Agent roles and runbooks

Canonical role instructions live in `roles/`. Codex TOML and Claude Markdown files are thin
native bindings that tell the agent to read this file and its role; do not duplicate the prompts.
Always read root `AGENTS.md` first. Paths in instructions are relative to the repository root.

## Choose only the role needed

| Native name in both clients | Use for | Role |
| --- | --- | --- |
| `sonar-scout` | Answer one bounded question from code or primary sources | [scout](roles/scout.md) |
| `sonar-designer` | Propose a design when a real boundary choice is open | [designer](roles/designer.md) |
| `sonar-implementor` | Implement an assigned change and focused checks | [implementor](roles/implementor.md) |
| `sonar-reviewer` | Find reproducible defects independently | [reviewer](roles/reviewer.md) |
| `sonar-validator` | Replay acceptance against the delivered artifact | [validator](roles/validator.md) |
| `sonar-judge` | Select among proposals when competing designs are useful | [judge](roles/judge.md) |

Models and effort are configured in the native bindings, not prescribed by a mandatory ladder.
Use a fresh reviewer for independent review; a judge must not have authored a proposal. Review
and design roles do not edit. Only the implementor edits assigned product paths; the validator
may create isolated test output but must not modify source, fixtures, or expected results.

## Dispatch

Give the child this compact packet, omitting fields that do not apply:

```text
Outcome / question:
Relevant files and consumer:
Write paths (or read-only), protected work, other active owners:
Decisions already made / invariants:
Acceptance and focused commands:
Resource, network, and spending limits:
Return: conclusion or changed paths, evidence, unresolved facts, next action.
```

Children are not alone in the checkout: preserve others' changes and never revert outside the
assigned lane. Root owns Git mutations, trackers, deployments, external writes, and further
delegation unless explicitly assigned otherwise. Child instructions do not broaden user authority.

## Ask questions that change the work

First inspect the relevant local owner, tests, and primary documentation. Ask yourself: which
missing fact could change correctness, scope, a public contract, cost, or a difficult-to-reverse
choice? Distinguish that from a routine implementation detail you can resolve and report.

Send material questions to the parent, not directly to the user. Include the precise unknown,
what you checked, why the answer matters, a recommended option, and the blocked work. Bundle
related questions; keep independent work moving. Optional choices may use a stated reasonable
assumption; a required answer or approval cannot be inferred from silence. The parent asks the
user only when repository/session evidence cannot resolve a material decision. Never re-ask for
authority the user already gave. These prompts guide investigation; do not recite a questionnaire.

## Handoff and close

Return concise findings or changed paths, commands with outcomes, unrun checks, and residual
uncertainty. Report net handwritten LOC when meaningful, without counting reference trees.
Then stop; no polling, unrelated follow-up, or self-assigned research. Parent records the useful
result, replays checks when needed, and closes/stops the child through the runtime's lifecycle
tool if one exists. If only completion is supported, verify it is completed and report that state;
do not claim a session was deleted. Stop abandoned active children before replacing them.

## Client setup and verification

See [.codex/README.md](../.codex/README.md) and [.claude/README.md](../.claude/README.md).
Role files are configuration, not proof that a running client loaded them. A fresh session must
discover the named role and execute a bounded read-only question before runtime use is proven.
No role enables permission bypass, installs software, provisions infrastructure, or runs by itself.
