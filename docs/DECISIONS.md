# Decisions

## 2026-10-23 — agent features and block recovery

From the whole-system review's ranked gaps (Linear SNR-16), built: block recovery, change tracking,
fit markdown with citation-style references, deterministic CSS-selector extraction, search time
filters and a news source, adaptive per-host backoff, durable crawl webhooks, and active-crawl
listing. Reviews and fixes followed (SNR-17, SNR-18).

Block recovery reverses the earlier "never escalate on a block" stance, following the owner's
direction to make the collector reach what a person can: a page that comes back blocked or
401/403/429 is retried once through the SOCKS5 residential route, then once in the browser, as
Firecrawl's `proxy: "auto"` does. Requests keep the declared user agent; nothing impersonates a
browser identity, so the open question of presenting as a normal browser is unchanged. Recovery is
on by default (`SONAR_BLOCK_RECOVERY`), honors `maxAge`, and records what ran in
`metadata.recovery`. Webhooks always go direct, never through the paid route.

## 2026-10-22 — browser council: supervised Chrome, isolated contexts, short sessions

Two blind designers proposed browser tiers; a fresh judge selected one and the root ratified it.
Selected: one supervised Chrome with a fresh isolated context per request (4 by default), plain
HTTP first with escalation only for script shells (fewer than 200 visible characters plus a concrete
shell marker; never on challenges or 401/403/429), a separate cache namespace for rendered pages,
no cache reads or shared execution for requests with actions, waits, or screenshots, no automatic
replay of actions, capture metadata recording the browser tier, and Firecrawl-compatible actions
(`wait`, `click`, `write`, `press`, `scroll`, `screenshot`, `scrape`, `executeJavascript`).

- Screenshots are served as expiring capability URLs under `SONAR_PUBLIC_URL` (as Firecrawl does),
  stored through the blob writer with a quota; the capability ID is never logged.
- Graft: short multi-step sessions (`POST /v1/browser`, `POST /v1/browser/:id`, `DELETE`), so an
  agent can look at a page before deciding what to click or type. Temporary state only; a Chrome
  restart invalidates them; bounded so they cannot take every tab.
- Confinement (root decision): on EC2, Chrome runs as its own systemd service whose per-unit IP
  filter allows only loopback, and `sonar` connects to it over the DevTools WebSocket
  (`SONAR_CHROME_WS`). Its only route out is the guarded forward proxy. Locally, `SONAR_CHROME_PATH`
  launches a child Chrome instead, without that guarantee.
- Rejected: shared execution for identical action requests, data-URL screenshots, silent no-op
  `waitFor` when the browser is off, and no memory ceiling.
- Reverses if measured demand makes four tabs the bottleneck: several supervised Chrome processes
  or a remote browser fleet behind the same interface.

## 2026-10-22 — crawl council: single-owner SQLite frontier

Two blind designers proposed crawl/map/batch designs; a fresh judge selected one and the root
ratified it. Selected: one `sonar` process owns `data_dir` (OS file lock); a SQLite frontier
(`jobs`, `frontier`, `documents`) with an in-memory per-host min-heap scheduler; each page's
document, counters, and discovered links commit in one transaction; restart re-queues leased rows
and fails rows after three crash attempts; status pages in completion order (`seq`); `sonar export`
streams the capture history as JSON Lines for fine-tuning. This drops the lease owner, expiry, and
generation fencing from the earlier plan: with a single owner they guard nothing.

- Graft: every frontier row records `capture_id` and `blob_sha256`, so each job result and export
  record points at its raw capture.
- Fixes required: cancel also stops in-flight completions; crawl headers are scoped to the seed
  host, cleared at job end, never exported; the gate takes the global slot before booking per-host
  spacing; `SONAR_PUBLIC_URL` for status `next` links; export records route and capture tier; a
  free-disk floor rejects new jobs.
- Rejected: lease fencing plus a single-owner lock (redundant), document blobs outside the
  transaction (orphans), a 32-request global default (contradicts 256).
- Reverses if more than one process or machine must share a queue: lease fencing, then Postgres.

## 2026-10-22 — build-out policy: all Rust, no tests or CI, push to main

The owner wants DripSonar fully in Rust, deployed as one binary on an EC2 host and run at high
throughput. For the build-out phase: no new tests, no CI, no pull requests or branch protection;
commit and push directly to the single `main` branch. Code still has to compile. The small tests
written before this decision stay but are not a gate. Headless Chrome (driven from Rust) remains
the only non-Rust runtime, for script-rendered pages; SearXNG stays an optional external endpoint.
This supersedes the `tools/ci` gate and stage-1 test list in ARCHITECTURE.md and the CI rule in
CODE_STYLE.md while the policy holds.

Build-out choices weighed the same day (crate metadata from crates.io, reqwest 0.12.28 and
hyper-util source, the pinned Spider checkout):

- Browser: `chromiumoxide` (async tokio, full DevTools protocol, most used). Rejected:
  `headless_chrome` (synchronous), `fantoccini`/`thirtyfour` (WebDriver cannot see subresources),
  the `chromey` fork (unneeded). All browser traffic goes through an in-process guarded forward proxy,
  because Chrome resolves DNS itself and a URL check alone leaves a rebinding window.
- PDF: `pdf_oxide` (pure Rust, MIT/Apache, markdown output; its benchmarks are the author's claims)
  in a killable child process. Fallback `pdf-extract` (documented panics). Rejected: `mupdf` (AGPL),
  `pdfium-render` (native library to ship; first upgrade if quality falls short).
- Residential proxy: SOCKS5 with local DNS only. reqwest passes our guarded resolver to SOCKS5 and
  sends an IP-literal CONNECT, then verifies TLS against the original host. HTTP-CONNECT proxies send
  the hostname and let the provider resolve it, so they are refused until a custom tunnel exists.
  Provider support for IP-literal SOCKS5 is unverified.
- Sitemaps on `quick-xml` + `flate2` (sitemap crates are stale); URL normalization on `url` plus a
  few rules; `texting_robots` later if robots.txt is re-enabled.

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
