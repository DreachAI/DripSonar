# Buildout

## First outcome

Given a company identity (preferably its domain and a registry identifier) and a research question,
return a bounded set of relevant, dated evidence and specific business signals. For example:
"What evidence supports that this company is opening a new location or advertising new roles?"
A job advertisement supports an advertised role; it does not by itself establish a hire,
headcount growth, or a customer's buying intent.

Each finding should preserve company identity and match rationale, claim, source URL, supporting
passage or structured record locator, content hash, fetched time, publication/event dates when
known, extraction method, and whether the conclusion is observed, inferred, ambiguous, or unsupported.
These are proposed field requirements; freeze a concrete schema with the first executable slice.

## Present state

Bootstrap guidance and research are present. Firecrawl is checked out as a pinned reference.
No Cargo package, crawler, search index, adapter integration, service, or deployment exists yet.

## Proposed delivery sequence

| Stage | Deliverable | Evidence required to call it done |
| --- | --- | --- |
| 0. Bootstrap | Agent rules, source pin, tool comparison, and buildout | Resolve local documentation links; validate Gitlink and clean reference checkout |
| 1. One company, known sources | Rust CLI takes company identity and explicit public URLs; fetches HTML or structured data and emits evidence records | Run a real public-source example plus deterministic fixtures; prove identity ambiguity, missing dates, errors, redirects, byte/time limits, and repeat-fetch behavior |
| 2. Bounded discovery | Domain map and crawl; scoped frontier, URL/content deduplication, stop controls, optional browser fallback | Crawl a controlled linked site; show depth/page budgets, no unbounded retry, cancellation, and no duplicate output; test private-network destination rejection |
| 3. First business signal | Port one selected `drip-kindness` source family and its filtering semantics | Fixture parity for the selected behavior, then one permitted live source; distinguish positive, irrelevant, ambiguous, stale, empty, and failed results with evidence |
| 4. Query-focused context | Search collected evidence, apply entity/date/source filters, return relevant excerpts with citations | A small labeled company-research set shows retrieval precision, missed evidence, identity errors, freshness, and citations; compare against a simple lexical baseline |
| 5. Repeatable collection | Incremental refresh, change detection, durable jobs, recovery, and service/API surface where needed | Restart without losing or duplicating work; prove content reuse, change history, budgets, auth/isolation, and retained evidence in the served user flow |

Stage 1 should be one complete vertical slice, not a scaffold of empty services. Select the first
source family by available public evidence and a real question, not by the size of the source catalog.
The above stages do not authorize new paid services or production changes beyond an active task.

## Starting design to evaluate

Company identity + question → source discovery → bounded fetch → document/record extraction →
evidence store → business-signal rules → query-focused context with source citations.

- Start HTTP-first. Compare Spider against a small fetch implementation before building a crawler
  framework. Browser and PDF execution can be isolated when the first sources require them.
- Keep the evidence artifact inspectable before choosing a database. Add a durable job store when
  restartable work is introduced; the source-of-truth store and a retrieval index have different jobs.
- Evaluate Tantivy for lexical retrieval over collected evidence. Introduce embeddings/reranking
  only if the labeled queries show a gap; a vector database is not a prerequisite for fetching pages.
- Treat general-web search as optional discovery. SearXNG aggregates upstream search; Exa and
  Context.dev are hosted provider candidates. None supplies proof that a company claim is true.
- Use Firecrawl for endpoint semantics, extraction stages, and job lifecycle ideas. Decide which
  contracts to preserve individually; do not promise full API parity or copy its hosted stack.

## `drip-kindness` port candidates

Source paths are relative to the separate `../drip-kindness` checkout. They identify mechanisms
to inspect and test, not dependencies currently imported by DripSonar.

| Source | Candidate behavior |
| --- | --- |
| `seeds/scrape_core.py` | Conditional requests, content hashes, bounded route retries, collection-window identity |
| `app-mirror/app/src/intel_collector/getter.py` | Shared transport boundary and fetch result contract |
| `app-mirror/app/src/intel_collector/generic_tabular_sources.py` | Structured rows and explicit field mappings |
| `app-mirror/app/src/intel_collector/generic_feed_sources.py` | Feed entries, date handling, and pagination |
| `app-mirror/app/src/intel_collector/generic_diff_sources.py` | Evidence-bearing changes between observations |
| `app-mirror/app/src/intel_collector/generic_walk_sources.py` | Bounded multi-page traversal and record extraction |
| `seeds/filter_core.py` | Explainable filtering and corroboration, keeping rejected evidence distinguishable |

Do not bulk-copy the endpoint catalog, legacy runtime/deploy wiring, private fixtures, or
historical agent procedures. Test the selected mechanism's actual behavior before claiming parity.

## Decisions to settle when their stage starts

- Stage 1 is now the Firecrawl replacement in [ARCHITECTURE.md](ARCHITECTURE.md); the signal
  stages below follow it. The owner still confirms crawler identity and license approach.
- Stage 2: browser need and crawl scheduling model, based on measured source requirements.
- Stage 4: local-corpus retrieval versus hosted discovery needs and data disclosure constraints.
- Stage 5: storage, runtime platform, and service boundary, based on actual volume and consumers.

No throughput, accuracy, latency, cost, or production-readiness claim is established by this plan.
