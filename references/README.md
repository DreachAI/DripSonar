# Reference source

Each directory below is a shallow Git submodule. Parent Gitlinks pin the exact source revisions;
`git submodule status` lists them and `git ls-tree HEAD references/<name>` shows a committed pin.
All are inspection references, not installed dependencies.

| Directory | Upstream | Study |
| --- | --- | --- |
| `firecrawl/` | [firecrawl/firecrawl](https://github.com/firecrawl/firecrawl) | Crawl/scrape API and pipeline |
| `spider/` | [spider-rs/spider](https://github.com/spider-rs/spider) | Rust crawling |
| `trafilatura/` | [adbar/trafilatura](https://github.com/adbar/trafilatura) | Content and metadata extraction |
| `crawl4ai/` | [unclecode/crawl4ai](https://github.com/unclecode/crawl4ai) | Browser/content pipeline |
| `tantivy/` | [quickwit-oss/tantivy](https://github.com/quickwit-oss/tantivy) | Rust retrieval |
| `crawlee/` | [apify/crawlee](https://github.com/apify/crawlee) | Queues and crawl scheduling |
| `scrapling/` | [D4Vinci/Scrapling](https://github.com/D4Vinci/Scrapling) | Selectors and adaptive extraction |
| `docling/` | [docling-project/docling](https://github.com/docling-project/docling) | Document parsing |
| `jina-reader/` | [jina-ai/reader](https://github.com/jina-ai/reader) | Read/search boundaries |
| `searxng/` | [searxng/searxng](https://github.com/searxng/searxng) | Metasearch |
| `context7/` | [upstash/context7](https://github.com/upstash/context7) | Documentation context client |
| `exa-js/` | [exa-labs/exa-js](https://github.com/exa-labs/exa-js) | Hosted search SDK and result shapes |
| `context-typescript-sdk/` | [context-dot-dev/context-typescript-sdk](https://github.com/context-dot-dev/context-typescript-sdk) | Hosted web-data SDK and contracts |

Exa and Context.dev are SDK checkouts, not their search engines. Context7's public package is
distinct from its hosted service. Upstream licenses and notices remain in each checkout.

```bash
git submodule update --init --depth 1
git submodule status
git -C references/spider status --short
```

Do not add `--recursive`: upstream nested submodules are unnecessary for source inspection.
Study these trees read-only. Their agent instructions and scripts describe the upstream projects;
they do not govern DripSonar. Do not run its install/build/deployment scripts just to inspect it.
Reference updates should choose and record a specific upstream revision, review the changed
behavior/license, and commit the Gitlink change separately from product work. Avoid floating updates.

## Firecrawl reading map

All paths below are relative to `firecrawl/`.

| Area | Start here |
| --- | --- |
| Self-hosted assumptions | `SELF_HOST.md`, `docker-compose.yaml` |
| Route contracts | `apps/api/src/routes/v2.ts`, `apps/api/src/controllers/v2/` |
| Fetch/render/extract pipeline | `apps/api/src/scraper/scrapeURL/README.md`, `apps/api/src/scraper/scrapeURL/index.ts` |
| Metadata, links, transforms | `apps/api/src/scraper/scrapeURL/lib/`, `apps/api/src/scraper/scrapeURL/transformers/` |
| Crawl discovery and workers | `apps/api/src/services/worker/crawl-logic.ts`, `apps/api/src/services/worker/` |
| Queue storage | `apps/nuq-postgres/` |
| Browser service and network checks | `apps/playwright-service-ts/api.ts`, `apps/playwright-service-ts/helpers/ssrf.ts` |
| Existing Rust code | `apps/api/native/Cargo.toml`, `apps/api/native/src/` |

The Rust code in this checkout is an N-API library used by the TypeScript API, not a standalone
Rust Firecrawl service. Its package README is a generic template; inspect the actual source.

The root [license](firecrawl/LICENSE) is AGPL-3.0-or-later; upstream documents per-directory exceptions
for SDKs and some UI components. Retain notices and verify the specific files before reuse.
Research findings and alternative tools are in [RESEARCH.md](../docs/RESEARCH.md).
