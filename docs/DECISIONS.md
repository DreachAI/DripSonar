# Decisions

## 2026-10-22 — small codebase and shared agent roles

The owner requested low LOC, high quality, and high modularity. Apply that to handwritten product
code through cohesive modules, narrow interfaces, reuse, and focused behavioral evidence. Do not
count reference checkouts as product size or compress code to meet an arbitrary numerical target.

Adapt Dreach's shared role-question pattern into six roles: scout, designer, implementor, reviewer,
validator, and judge. Canonical prompts and dispatch/clarification/closure runbooks live in `.agents/`.
Native Codex and Claude bindings read those files instead of duplicating them, so no generator or
full model ladder is needed. The roles guide useful work; no mandatory council/review ceremony is added.

Native format sources are the [Codex subagent docs](https://learn.chatgpt.com/docs/agent-configuration/subagents)
and [Claude Code subagent docs](https://code.claude.com/docs/en/sub-agents), checked 2026-10-22.
Local definition checks do not establish runtime discovery or model access. No global client
settings, permission bypasses, API credentials, or automatic agent jobs are introduced.

Follow-up source verification: GitHub identifies the Drip repository as `DripYCx26/dreach`;
its remote HEAD on 2026-10-22 matches `f0ac4595ca65be72252e7e219aeb2cec8d257d48`.
Read its `.agents/shared.md`, role prompts, and `dreach-research`, `dreach-design`, and
`dreach-delivery` workflow skills directly. Adapt their evidence stopping rule, deletion/reuse
questions, scope-and-acceptance handoff, actionable complexity review, and narrow repair rechecks.
Keep DripSonar's shared prompt references instead of importing Dreach's generator and model ladder.

## 2026-10-22 — purpose and first user flow

The owner selected company research and specific business signals as the first use case.
DripSonar combines a Rust implementation of selected Firecrawl capabilities with selected
`drip-kindness` collection methods. Full Firecrawl API parity, hosted search providers, and a
particular database or deployment platform have not been selected.

The current task establishes guidance, references, and research. Later build stages in
[BUILDOUT.md](BUILDOUT.md) are proposals, not statements of implemented behavior.

## 2026-10-22 — adapt relevant Dreach rules

The accessible `DreachAI` GitHub org listing contained only DripSonar at inspection. The existing
Dreach checkout at `../dreach` points to `DripYCx26/dreach`; it is the source used for project
conventions, not an assumed organization-wide policy repository.

Reviewed a clean local Dreach checkout at `f0ac4595ca65be72252e7e219aeb2cec8d257d48`:

- `AGENTS.md` and `README.md`: short agent contract and selective documentation loading.
- `docs/operations/CODE_STYLE.md`: reuse, one owner per rule, mechanical formatting, typed errors,
  purposeful comments, focused behavioral checks, and evidence-based performance claims.
- `docs/operations/agent-workflow.md`: bounded ownership and explicit validation evidence.
- `docs/PLAN.md` and `docs/ARCHITECTURE.md`: staged buildout and named module responsibilities.

Adopt those practices in DripSonar's own context. Do not import the staffing/VMS product scope,
PostgreSQL-as-engine mandate, Azure deployment, Linear IDs, schema lifecycle, or mandatory agent
ladders. The owner's current working instructions make reviews and councils optional tools.
These are local source snapshots, not a claim that upstream HEAD is unchanged.

## 2026-10-22 — collection-source boundary

Reviewed `drip-kindness` at clean local revision `ae4c7bfca506475b524124712416e33c98aea09e`:
`AGENTS.md`, the fetch/filter module descriptions, and the generic adapter interfaces.
Useful mechanisms are conditional fetches, content deduplication, data-driven source families,
explicit missing data, and evidence-bearing filter decisions. These reads do not prove runtime
behavior. Preserve the source repository's catalog and private-data exclusions; no catalog,
fixture payload, runtime state, or script source is copied in this bootstrap.

## 2026-10-22 — upstream references

Firecrawl is a pinned Git submodule at `references/firecrawl`, with its original license and
notices intact. The Gitlink is the authoritative revision. Do not vendor its tree into DripSonar
source or install its runtime as part of a reference update. The owner's follow-up requested the
other research references too: their open repositories and hosted SDKs are pinned under `references/`.
Do not initialize their nested submodules or install/build them as part of source inspection.

No DripSonar distribution license has been selected. Record the source and license for any code
copied or translated; Firecrawl's root AGPL license and per-directory exceptions must be considered
when selecting the port's code-reuse and distribution approach.
