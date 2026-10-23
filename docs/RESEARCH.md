# Collection and context research

Checked 2026-10-22 against upstream repositories and official documentation. This is source
inspection, not a benchmark. Recommendations below are judgments about fit for company research;
vendor performance claims are not measured DripSonar results.

## Where to start

1. **Firecrawl for the product contract; Spider for a Rust crawling candidate.** Compare HTTP-first
   collection, bounded discovery, and browser fallback before translating a large TypeScript stack.
2. **Trafilatura and Crawl4AI for extraction comparisons.** Judge whether the relevant passage,
   table, date, and source locator survive; attractive Markdown alone is not success.
3. **Selected `drip-kindness` methods for business signals.** Conditional collection, source-family
   parsing, change detection, and explainable filtering connect generic web data to a company question.
4. **Tantivy for searching the evidence we own.** Build a lexical baseline before adding model
   ranking. Exa and Context.dev are useful references for live discovery and cited result design.

These are candidates to evaluate, not dependencies selected or installed.

## Collection and extraction

| Tool | Available implementation / license | What to study for DripSonar | Boundary |
| --- | --- | --- | --- |
| [Firecrawl](https://github.com/firecrawl/firecrawl) | Source available under [AGPL-3.0-or-later](../references/firecrawl/LICENSE); upstream documents exceptions for SDKs and some UI components | Separate scrape, map, crawl, search, and async job contracts; staged extraction and browser isolation | Our pinned self-host source is not proof of hosted feature parity; see inspection below |
| [Spider](https://github.com/spider-rs/spider) | Rust crawler; [MIT](https://github.com/spider-rs/spider/blob/main/LICENSE) | HTTP-first streaming crawl, configured depth/budgets, optional Chrome; most direct Rust engine candidate | Local crate and managed Spider Cloud are distinct deployment choices; test the selected feature set without a cloud dependency |
| [Crawl4AI](https://github.com/unclecode/crawl4ai) | Python crawler/extractor; [Apache-2.0](https://github.com/unclecode/crawl4ai/blob/main/LICENSE) | Browser orchestration and clean content output; compare extraction results on company pages | Reference implementation or comparison runner, not a Rust core; cloud offering is separate |
| [Crawlee](https://github.com/apify/crawlee) | TypeScript crawling library; [Apache-2.0](https://github.com/apify/crawlee/blob/master/LICENSE.md) | Persistent request queue, HTTP/browser interface, resource-aware concurrency, error/retry hooks | Useful scheduling design; do not import Apify's hosted platform as a prerequisite |
| [Scrapling](https://github.com/D4Vinci/Scrapling) | Python scraping framework; [BSD-3-Clause](https://github.com/D4Vinci/Scrapling/blob/main/LICENSE) | Adaptive element tracking, selector recovery, structured exports, crawl resume | A recovered selector can match the wrong fact; validate identity and evidence on changed layouts |
| [Trafilatura](https://github.com/adbar/trafilatura) | Python content/metadata extraction; [Apache-2.0](https://github.com/adbar/trafilatura/blob/master/LICENSE) for current versions | Main text, metadata, and boilerplate removal; comparison baseline for news and company announcements | Historical releases before 1.8.0 had GPLv3+ licensing; pin the actual version used |
| [Docling](https://github.com/docling-project/docling) | Document parsing; [MIT code](https://github.com/docling-project/docling/blob/main/LICENSE), model licenses separate | PDF layout, reading order, tables, and structured document output for filings/reports | Introduce only when document-heavy sources justify the runtime/model cost; verify each model's terms |
| [Jina Reader](https://github.com/jina-ai/reader) | Open reader/search branch; [Apache-2.0](https://github.com/jina-ai/reader/blob/main/LICENSE) | Simple URL-to-readable-content boundary, separating read from search, cache options | README says SaaS MongoDB storage is excluded; do not assume hosted and self-hosted implementations are identical |

## Search and focused context

| Tool | What is open / hosted | Useful idea | Fit for company signals |
| --- | --- | --- | --- |
| [Tantivy](https://github.com/quickwit-oss/tantivy) | Rust search library, [MIT](https://github.com/quickwit-oss/tantivy/blob/main/LICENSE) | Local lexical indexing and queries over collected documents | Strong candidate for exact company names, domains, phrases, and source metadata. It provides neither a web corpus nor a ready-made search server |
| [SearXNG](https://github.com/searxng/searxng) | Self-hosted metasearch, AGPL-3.0 | Normalize results from several search providers; [HTTP search API](https://docs.searxng.org/dev/search_api.html) | Candidate discovery service. Still depends on external engines; JSON must be enabled, and public instances may disable it |
| [Exa](https://exa.ai/docs/reference/search) | Hosted search API; [official JavaScript SDK](https://github.com/exa-labs/exa-js) is MIT | Query-focused highlights with URLs, page text, titles, and publication dates | Useful product/API reference and optional discovery provider. An open SDK is not an open search index or engine |
| [Context.dev](https://www.context.dev/) | Hosted web-data API; [public organization](https://github.com/context-dot-dev) publishes SDKs | Search with content, scrape/crawl, cited answers, and change monitors as one workflow | Relevant to company monitoring. No public engine was identified in the reviewed sources; do not label it a self-hostable open-source alternative |
| [Context7](https://github.com/upstash/context7) | MIT MCP package; separate [on-premise terms](https://github.com/upstash/context7/blob/master/docs/enterprise/on-premise.mdx) | Resolve a library identity/version before retrieving its documentation | Secondary inspiration only: primarily software documentation, not company/business research. Context7 is distinct from Context.dev |

Open SDKs, open engines, model weights, and paid hosted services are different artifacts. Preserve
that distinction when selecting dependencies. None of these hosted APIs has been called with our
private source data or wired into DripSonar by this task.

## What the Firecrawl checkout actually contains

Inspected revision `e0b48886c3f334864b00f1fd62c211fd51ac2910`; see the
[reference reading map](../references/README.md) for local paths. The parent Gitlink owns future pins.

- The v2 router and controllers provide the API contract starting points. Scrape engine selection,
  metadata/links, cleanup, and format transforms are separable stages worth understanding.
- The API already has Rust N-API helpers in `apps/api/native/`; these are a TypeScript-hosted
  library, not an existing Rust server to rename. Inspect them before assuming every parser must
  be written from scratch.
- Crawl discovery and scrape workers are separate. NuQ has a PostgreSQL path and an optional
  FoundationDB backend; neither backend is automatically required by DripSonar.
- `SELF_HOST.md` defaults to bundled Playwright plus basic fetch, with separate engines and model
  providers optional. Its Compose baseline starts with authentication disabled and lacks persistent
  volumes for the named queue/cache stores. It is not a production design to inherit unchanged.
- The v2 extract route is marked deprecated in the inspected source. Do not freeze compatibility
  around every historical endpoint without choosing the behavior we need.

No upstream code, dependencies, services, or browser sessions were executed for this inspection.

## How to compare the candidates

Use a small public or explicitly permitted company-research corpus, retaining source snapshots
and expected evidence so every candidate sees the same input. Include a company announcement,
a careers page, a structured feed/table, and a PDF only if the first target requires it. Include
hard negatives: a namesake company, an old repost, a navigation-only page, no matching evidence,
an ambiguous date, and a failed request.

Measure entity-match correctness, supporting-passage retention, source/date attribution, false
signals, missed evidence, unchanged-page reuse, bytes/requests, browser invocation rate, and
latency with errors included. For retrieval, label relevant evidence per question and report
precision/recall at a stated cutoff. For company signals, judge the claimed business event
separately from whether the page fetched successfully.

Start with a company domain, a concrete question, and explicit source URLs. Discover more sources
only when that flow needs them. Compare a straightforward lexical extractor/retriever before
adding a vector database, agent loop, or LLM reranker. The first implementation stage is defined
in [BUILDOUT.md](BUILDOUT.md); none of these comparison runs has been performed yet.
