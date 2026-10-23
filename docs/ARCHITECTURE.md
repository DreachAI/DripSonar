# Architecture

Locked design for DripSonar's first deliverable: a durable, self-hosted Rust replacement for the
Firecrawl and context.dev endpoints that AI research tools already call. Decisions and rejected
alternatives are in [DECISIONS.md](DECISIONS.md) (2026-10-22 entries). Nothing here is built yet;
every behavior below is a requirement, and live checks are NOT-RUN until reported otherwise.

## Consumer

`drip-training`'s `src/drip_training/tasks/staffing/research_tools.py` gives models three tools:
`web_search`, `read_page`, and `find_contact` (a search). Its backends call either self-hosted
Firecrawl at `DRIP_FIRECRAWL_URL` (default `http://127.0.0.1:3002`) or context.dev. DripSonar
must serve both wire shapes so either backend setting works unchanged:

| Endpoint | Request the consumer sends | Response fields it reads |
| --- | --- | --- |
| `POST /v1/scrape` | `{url, formats:["markdown"], onlyMainContent:true, timeout:25000}` | `success`, `data.markdown`, `data.metadata.statusCode`; on non-2xx, body `code` (`SCRAPE_*`) |
| `POST /v1/search` | `{query, limit:10}` | `success`, `data[].{url,title,description}` |
| `POST /v1/web/scrape` | `{url, formats:{markdown:true}, sharedParams:{mainContentOnly:true}}` | `markdown.{data,success,requested,code,status,error}` |
| `POST /v1/web/search` | `{query, numResults:10, queryFanout:false}` | `results[].{url,title,description}`; `X-Credits-Used` absent, `0`, or `1` |

It treats `SCRAPE_ALL_ENGINES_FAILED`, `SCRAPE_SITE_RESTRICTION_BLOCKED`, and the anti-bot codes as
blocked, a page `statusCode` of 401/403/429 as blocked, and any schema mismatch as an error. It
caps responses at 2 MiB and page text at 16,000 characters itself.

Firecrawl contracts are reimplemented from observed behavior (its source is AGPL; no code copied).
v1 is the compatibility target; v2 routes and crawl/map follow in later slices.

## Shape

One Cargo package, `dripsonar`: a library plus the `sonar` binary (`serve`, `scrape`, `search`).

| Module | Owns | Interface | Depends on |
| --- | --- | --- | --- |
| `config` | Settings from flags and environment, validated once | `Config` | — |
| `net` | Destination guard, DNS pinning, redirects, deadlines, byte caps, decoding, retries, per-host gate | `Fetcher::fetch(&Target) -> Result<Fetched, FetchError>` | config |
| `page` | Pure HTML transforms: cleanup, main content, markdown, metadata, links | `render(&Fetched, &RenderOptions) -> Rendered` | — |
| `store` | SQLite metadata and content-addressed blob files; capture cache | `Store::{cached, put}` | net (types only) |
| `scrape` | One scrape: cache, single-flight, fetch, classify, render | `Scraper::scrape(&ScrapeRequest) -> Result<Document, ScrapeError>` | net, page, store |
| `search` | Search providers and optional per-result scraping | `Searcher::search(&SearchRequest) -> Result<Vec<Hit>, SearchError>` | net, scrape |
| `api` | HTTP routes, both wire shapes, validation, auth, error mapping | `router(AppState) -> axum::Router` | scrape, search |
| `main` | CLI and server lifecycle | — | api, config |

Dependencies are acyclic in that order. `page` never touches the network or disk. `net::fetch` is
the only network path and always applies the guard internally, so no caller can skip it; there is
no proxy route yet, so no separate egress module exists until one is needed.

## Network rules (`net`)

- Schemes `http`/`https`, ports 80/443 and any explicitly configured extra ports; no credentials in
  URLs. Missing scheme gets `http://`, matching Firecrawl.
- Every resolved address must be public unicast. Refused: loopback, RFC 1918, link-local including
  `169.254.169.254`, `100.64.0.0/10`, `0.0.0.0/8`, multicast, broadcast, documentation and benchmark
  ranges, IPv6 unique-local and link-local, and IPv4-mapped or -compatible IPv6 judged on the inner
  address. A mixed public/private answer is refused.
- The check runs in a custom reqwest resolver, so the connection uses the checked addresses (no
  rebinding window). IP-literal hosts bypass resolvers, so the guard also checks them directly.
- The client is built with `.no_proxy()` and redirect policy `none`. Redirects are followed manually,
  at most 5, each hop re-guarded; HTTPS→HTTP downgrade is refused.
- One deadline covers the whole exchange including redirects and retries (request `timeout`,
  default 30 s, server maximum 120 s). Bodies are capped at 10 MiB after decompression; gzip, deflate,
  and brotli are decoded under the same cap.
- Text is decoded with the charset from the header, then `<meta charset>`, then UTF-8 lossy.
- Retries: one retry on connect error or reset, within the deadline. `429`/`503` with `Retry-After`
  within the remaining deadline are retried once; otherwise the response is returned as observed.
- Politeness: a global concurrency limit (default 32) and per-host limit (default 2) with minimum
  spacing (default 250 ms) per host. Waiting counts against the request deadline.
- Identity: a declared user agent, `DripSonar/<version> (+<contact URL>)`, configurable.
- robots.txt is not consulted (owner decision for testing; see DECISIONS.md).

## Pages (`page`)

- Remove `script`, `style`, `noscript`, `meta`, `head` content from the rendered body; apply
  `includeTags` (keep only matches), then `excludeTags`.
- `onlyMainContent` removes Firecrawl's documented non-main selectors (`header`, `footer`, `nav`,
  `aside`, and the class/id list for headers, footers, sidebars, modals, ads, language selectors,
  social, menus, breadcrumbs, share widgets, and cookie banners) unless the element contains `#main`.
- `removeBase64Images` replaces `data:` image sources. Links and image sources are made absolute.
- Markdown via a maintained HTML-to-Markdown crate (GFM tables, inline links).
- Metadata: `title`, `description`, `language`, `keywords`, `robots`, `favicon`, Open Graph,
  `article:published_time`/`modified_time` as `publishedTime`/`modifiedTime`, Dublin Core date
  fields, plus `sourceURL`, `url` (final), `statusCode`, `contentType`, `proxyUsed:"basic"`,
  `cacheState`, `cachedAt`. Dates are copied as the page states them; fetch time never fills them.
- Non-HTML: `text/plain` and JSON pass through as text; PDFs and other binaries return
  `SCRAPE_UNSUPPORTED_FILE_ERROR` until a document parser is added.

## Scrape (`scrape`)

1. Validate and normalize the URL; build the cache key from the normalized URL and request headers.
2. If a capture younger than `maxAge` exists (default 1 day; `0` disables), render from it
   (`cacheState:"hit"`). Rendering is cheap and always recomputed from stored bytes.
3. Identical concurrent misses share one fetch (single-flight).
4. Fetch, then store the blob and capture row before rendering (blob: temp file, fsync, rename;
   then the row in one transaction). A crash leaves at most an orphan blob.
5. Classify: a challenge interstitial (for example `cf-mitigated: challenge`, or a small HTML body
   with a known challenge title) is `Blocked`; other 4xx/5xx pages render normally and report their
   `statusCode`, as Firecrawl does.

Error mapping (Firecrawl v1 body `{success:false, code, error}`):

| Failure | Code | HTTP |
| --- | --- | --- |
| Invalid body or unknown key | `BAD_REQUEST` | 400 |
| Guard refusal (private address, scheme, port) | `SCRAPE_SITE_ERROR` | 403 |
| DNS failure | `SCRAPE_DNS_RESOLUTION_ERROR` | 200 |
| Deadline exceeded | `SCRAPE_TIMEOUT` | 408 |
| TLS failure | `SCRAPE_SSL_ERROR` | 500 |
| Connect, reset, too many redirects, oversized body | `SCRAPE_SITE_ERROR` | 500 |
| Challenge or block page | `SCRAPE_ALL_ENGINES_FAILED` | 500 |
| Unsupported content type | `SCRAPE_UNSUPPORTED_FILE_ERROR` | 500 |
| Server overloaded past the queue deadline | `CONCURRENCY_QUEUE_TIMEOUT` | 408 |

The context.dev shape reports the same outcome inside `markdown`: success sets `data`,
`requested:true`, `success:true`; failure sets `data:null`, `success:false`, and `code` (`BLOCKED`
for blocks, otherwise the Firecrawl code) with `error`.

## Search (`search`)

- Providers in order: SearXNG when `SEARXNG_ENDPOINT` is set (`/search?format=json`), then
  DuckDuckGo's HTML endpoint. Each returns ranked `{url, title, description}`; results are
  deduplicated by normalized URL and cut to `limit` (Firecrawl default 5, max 100; context
  `numResults`, max 100). `queryFanout` is accepted and ignored.
- A provider failure falls through to the next; if all fail, the response is an error, never an
  empty success. A genuine zero-result search is an empty success with Firecrawl's warning text.
- Search engine requests use the same `net` fetcher and guard.
- `scrapeOptions.formats` non-empty scrapes each hit through `scrape` with bounded concurrency.

## Storage (`store`)

- `<data_dir>/sonar.db` (SQLite, WAL, foreign keys) with a `captures` table: id, cache key,
  requested URL, final URL, status, selected headers, content type, blob hash, byte length,
  fetched_at. `<data_dir>/blobs/sha256/<ab>/<hash>` holds raw bodies.
- The database is the cache index and the audit log. Captures are appended, never updated.
- A size-bounded sweeper may delete captures older than a retention window and unreferenced blobs.

## API (`api`)

- Routes: `/v1/scrape`, `/v1/search`, `/v1/web/scrape`, `/v1/web/search`, `GET /health`.
- Request bodies deny unknown fields and are capped at 1 MiB. Fields Firecrawl accepts but this
  slice does not implement (`actions`, `json`/`extract` formats, `screenshot`, `location`) are
  rejected with `BAD_REQUEST` naming the field rather than silently ignored; `waitFor`, `mobile`,
  `blockAds`, `proxy`, and `storeInCache` are accepted and recorded as no-ops where harmless.
- Auth: when `SONAR_API_KEY` is set, `Authorization: Bearer <key>` is required; otherwise keyless,
  like self-hosted Firecrawl.
- Logs carry request IDs, routes, status codes, codes, byte counts, and timings, never page text,
  queries, or credentials.
- Graceful shutdown stops accepting requests and lets in-flight requests finish within a bound.

## Stage-1 tests (written first, kept few)

| Test | Proves |
| --- | --- |
| `guard_table` | Address, scheme, port, IP-literal, mapped-IPv6, and metadata cases |
| `redirects_are_reguarded_and_capped` | A redirect to a private address is refused; the sixth hop fails |
| `limits_fail_closed` | Oversized and slow bodies fail with the right codes |
| `page_render_fixture` | Main content excludes navigation and footer, keeps `#main`, metadata and absolute links |
| `api_wire_shapes` | Both scrape and search shapes, strict 400s, and the error table above |
| `cache_and_single_flight` | A cached hit avoids a refetch; `maxAge:0` refetches; concurrent misses fetch once |

Tests use a local HTTP server and an injected resolver through a test-only constructor that is not
compiled into the release binary. `tools/ci` runs `cargo fmt --check`, `cargo clippy --all-targets
--locked -- -D warnings`, and `cargo test --locked`. The live check runs `sonar serve` and points
`drip-training`'s firecrawl backend at it for a handful of real `read_page` and `web_search` calls.

## Next slices

1. **Crawl and map (durable jobs):** `POST /v1/map`, `POST /v1/crawl`, `GET`/`DELETE /v1/crawl/:id`.
   A `jobs` table and a `frontier` table in the same SQLite file; workers lease frontier rows with a
   generation token, so a restart resumes and stale workers cannot commit. Sitemaps, scope rules,
   depth and page limits, per-host interleaving, and cancellation.
2. **Browser tier:** a separately supervised headless browser used only when a page is a script
   shell, with all its traffic forced through the same guard.
3. **Proxy route:** per-host permission, route recorded on every capture, spend caps; tunnels must
   connect to the locally validated address or the provider is refused.
4. **Documents:** PDF text extraction behind the same `page` interface.
5. **v2 routes**, then the company-signal layers in [BUILDOUT.md](BUILDOUT.md).
