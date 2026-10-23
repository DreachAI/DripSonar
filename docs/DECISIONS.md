# Decisions

## 2026-10-22 — first deliverable: durable Firecrawl replacement for AI tools

The owner redirected the first deliverable to a durable Rust port of Firecrawl, with upgrades,
for AI agents to call as a tool. `drip-training`'s research tools already call self-hosted
Firecrawl (`/v1/scrape`, `/v1/search`) or context.dev (`/v1/web/scrape`, `/v1/web/search`), so
DripSonar serves both wire shapes and replaces either backend without client changes.
[ARCHITECTURE.md](ARCHITECTURE.md) is rewritten around that slice; crawl/map jobs, a browser tier,
proxy routing, documents, and v2 routes follow. The Greenhouse signal slice moves after them.
This supersedes "do not promise full API parity" for these four endpoints only.

- robots.txt is not consulted for now, for testing (owner decision). Revisit before any shared or
  production use.
- Whether DripSonar ever presents as a normal browser stays open while the owner researches it.
  Nothing implements impersonation; requests carry a declared user agent.
- Firecrawl's size for scale: about 373k lines of code across the repository at the pinned
  revision, about 165k in its non-test API server (22k of which is one model-price table), plus
  about 115k lines of API tests (`wc -l`, including blanks and comments).

## 2026-10-22 — architecture council

The owner asked for a design council on the whole architecture. Three blind designers proposed
from the same intake (repository docs, pinned references, `drip-kindness` at `ae4c7bf`, and the
practitioner digest in [RESEARCH.md](RESEARCH.md)); a fresh judge scored them anonymized on a fixed
rubric and selected one without blending. The root spot-checked the judge's central claims:
Spider fetches its seed URL outside its own redirect SSRF guard (`spider/src/website.rs`), bundled
`libsqlite3-sys` 0.37 enables FTS5, reqwest 0.12.28 exposes `no_proxy` and `dns_resolver`, and
`drip-kindness`'s getter ignores robots `Disallow` by its owner's ruling.

Selected: a single Rust package with a type-enforced egress seam (only `egress` can authorize a
fetch), SQLite metadata plus content-addressed blob files, structured sources before HTML before a
browser, escalation only by explicit per-host permission, honest declared identity by default, and
Greenhouse job boards as the first source family. The next entry moved the first deliverable to a
Firecrawl replacement; this design's foundations carry over into [ARCHITECTURE.md](ARCHITECTURE.md).

- Graft from a losing proposal: proxied routes must tunnel to the locally validated address or fail
  closed, because a residential exit sits on a third party's network.
- Repairs before implementation: no `egress`↔`net` cycle; content-derived claim IDs so reruns are
  idempotent; disable ambient proxies and check IP-literal hosts; blob-then-row write order; no
  `egress`→`store` dependency in stage 1; stage-1 lifecycle limited to baseline and no inference
  from failed captures. SQLite in stage 1 supersedes "inspectable artifact before a database";
  JSON Lines export plus readable blob files keep evidence inspectable.
- Rejected: files-and-manifest storage first (forces a stage-5 rewrite of repeat-fetch state);
  an all-in-SQLite design that also banned impersonation outright and required numeric-address
  proxies before the owner decided; Spider as the crawl core; Firecrawl-shaped services; orchestration
  over hosted Firecrawl or Exa; a Python extraction sidecar from the start.
- Owner decisions pending, with conservative defaults until decided: robots `Disallow` honored;
  impersonation off; proxy off in stage 1; GitLab on Greenhouse as the first live target; crawler
  identity and contact; raw-capture retention; license; language-model use and budget.

Residual risk: Greenhouse field meanings may drift; Greenhouse robots.txt and terms are unchecked;
the honest-identity default may lower coverage and must stay visible in reports.

## 2026-10-22 — reviewer bar and efficient delivery

The owner explicitly requires a 97/100+ bar for every reviewer, short high-quality tests, minimal
gates, removal of useless code, and modular implementation. `CODE_STYLE.md` owns the acceptance
rule: both correctness and fitness must meet the bar, supported by evidence. Material unresolved
defects still block acceptance. This supersedes the earlier omission of a numerical review floor;
it does not add review quotas, unsupported scores, or repeated checks on unchanged work.
The owner clarified that this standard starts during design and applies throughout delivery:
strict, unbiased assessment; minimal owned complexity; and no score inflation or performative
harshness. Design proposals must earn their moving parts before implementation starts.

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
