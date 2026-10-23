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

## Stage-1 tests (written before the no-tests policy; kept, not a gate)

| Test | Proves |
| --- | --- |
| `guard_table` | Address, scheme, port, IP-literal, mapped-IPv6, and metadata cases |
| `redirects_are_reguarded_and_capped` | A redirect to a private address is refused; the sixth hop fails |
| `limits_fail_closed` | Oversized and slow bodies fail with the right codes |
| `page_render_fixture` | Main content excludes navigation and footer, keeps `#main`, metadata and absolute links |
| `api_wire_shapes` | Both scrape and search shapes, strict 400s, and the error table above |
| `cache_and_single_flight` | A cached hit avoids a refetch; `maxAge:0` refetches; concurrent misses fetch once |

Tests use a local HTTP server and an injected resolver through a test-only constructor that is not
compiled into the release binary. There is no CI gate during the build-out (DECISIONS.md). The live
check, when the owner runs it, points `drip-training`'s firecrawl backend at `sonar serve`.

## Build-out slices

Choices below were weighed on 2026-10-22 from crate metadata and reference source; see
DECISIONS.md. Each slice is a module with one owner; `api` gains routes, `scrape` gains tiers.

### Crawl, map, and batch scrape (`jobs`, `crawl`, `sitemap`)

- Routes: `POST /v1/map`, `POST /v1/crawl`, `GET`/`DELETE /v1/crawl/:id`, `POST /v1/batch/scrape`,
  `GET`/`DELETE /v1/batch/scrape/:id`, with Firecrawl v1 fields and defaults (crawl `limit` 10000,
  `maxDepth` 10, `includePaths`/`excludePaths` regexes, `allowBackwardLinks`/`crawlEntireDomain`,
  `allowExternalLinks`, `allowSubdomains`, `ignoreSitemap`, `ignoreQueryParameters`, `delay`,
  `scrapeOptions`; map `limit` 5000, `search`, `includeSubdomains`, `sitemapOnly`, `ignoreSitemap`).
- One serving process owns `<data_dir>/sonar.lock` through an OS file lock. `jobs::db` owns SQL
  for `jobs`, `frontier`, and `documents`, using `Store`'s writer transactions and pooled readers.
  The frontier stores task kind, URL identity, depth, attempts, completion sequence, errors,
  `capture_id`, and `blob_sha256`; documents are serialized domain values, not wire responses.
- Leasing marks the oldest queued row for a host. A page's document, counters, and discovered
  links commit together, only while both its lease and job remain active. Cancellation flips
  queued and leased rows; late completions are discarded. Restart re-queues leases, failing rows
  with three attempts as `SCRAPE_CRASH_LOOP`; no lease expiry, owner, or generation is needed.
- A bounded worker pool (192 by default) and per-host minimum-time heap interleave hosts using
  the same concurrency and spacing settings as `net`, plus crawl `delay`. The network gate stays
  authoritative. Shutdown stops dispatch and drains workers within the configured request timeout.
- Discovery: links from each rendered page plus `sitemap.xml` (and sitemap indexes, gzip) parsed
  with `quick-xml` and `flate2` under byte, entry, and depth caps. Every discovered URL is normalized
  (lowercase scheme and host, drop fragment and default port, optionally drop the query) and passes
  the scope rules before insertion; inserts are `INSERT OR IGNORE`, so duplicates cost nothing.
- Map runs sitemap plus one page of links (no full crawl), filters by `search` substring, and caps.
- Status pages successful documents in stable completion order, up to 10 MiB (at least one
  document), with `next`, `completed`, `total`, `creditsUsed: 0`, and `expiresAt`. `next` uses
  `SONAR_PUBLIC_URL` when configured, otherwise the request Host header. Crawl errors have their
  own endpoint. A ten-minute sweeper deletes expired terminal jobs with cascading documents;
  retention defaults to 24 hours after completion. Raw captures remain independent of jobs.
- Crawl headers are sent only to the seed host, removed from stored specifications at job end,
  and never included in status or export records. Webhooks and full-URL regexes are rejected by
  name; `maxConcurrency` and `deduplicateSimilarURLs` are accepted no-ops. Batch scrape options
  are top-level. New jobs are refused at the active-job ceiling or below the free-disk floor
  (`SONAR_MIN_FREE_DISK_MB`, default 2048; HTTP 507).
- `sonar export [--since RFC3339] [--host SUFFIX] [--out PATH]` streams captures as JSON Lines,
  using the shared capture renderer, filtering failed/blocked/unsupported/empty pages, and
  deduplicating markdown hashes. Records include capture and blob IDs, source dates as stated,
  route, and `tier: "http"`. Existing output files are never overwritten. The deduplication set
  grows with unique exported documents; export does not read job specifications or headers.

### Browser tier (`browser`)

- Crate: `chromiumoxide` (async tokio, full DevTools protocol). One Chrome process, a semaphore of
  N tabs (default 4), a per-page timeout, restart every 200 pages or on any handler error, child
  killed on drop, a fresh `--user-data-dir` per launch. Disabled unless `SONAR_CHROME_PATH` is set.
- Escalation: plain HTTP first. The browser runs only when the request asks (`waitFor` > 0 or
  `actions`), or when the plain response is HTML whose rendered text is tiny but whose body carries
  script-shell markers (a root `div` with no text, `__NEXT_DATA__` without content, `noscript`
  warnings). Never on a block or challenge.
- Guard: Chrome is launched with `--proxy-server=http://127.0.0.1:<port>` and
  `--proxy-bypass-list=<-loopback>`, pointing at an in-process forward proxy (`egress`) that resolves,
  checks, and dials the checked address (directly or through the SOCKS5 route), and refuses
  everything else. WebRTC non-proxied UDP is disabled. The rendered HTML goes through `page` as usual.

### Egress and proxy route (`egress`)

- Upstream residential proxy: `SONAR_PROXY=socks5://user:pass@host:port`, only `socks5://` (local
  DNS through our guarded resolver, IP-literal CONNECT, TLS verified against the original host).
  `socks5h://` and `http(s)://` proxies are refused at startup: they let the proxy resolve names.
- Route per destination host from `SONAR_PROXY_HOSTS` (comma-separated suffixes, or `*`); default
  is direct. Every capture records `proxyUsed: "basic"` (direct) or `"stealth"` (proxied) and the
  bytes moved; `SONAR_PROXY_MAX_BYTES_PER_DAY` stops proxied fetches when spent.
- `egress` also hosts the local forward proxy used by the browser (HTTP `CONNECT` and absolute-URI
  requests), bound to loopback with a random port and a per-launch token in the proxy credentials.

### Documents (`document`)

- Crate: `pdf_oxide` (MIT/Apache, pure Rust, markdown output). Extraction runs in a child process,
  `sonar extract-pdf`, fed bytes on stdin and killed after a timeout (default 30 s), with page and
  output caps; a panic or hang in a parser cannot take down the server. A PDF with almost no text
  per page returns `SCRAPE_PDF_OCR_REQUIRED`; encrypted PDFs return `SCRAPE_UNSUPPORTED_FILE_ERROR`.
- Metadata from the PDF info dictionary maps to `title`, `numPages`; dates only as stated.

### v2 routes

- `/v2/scrape`, `/v2/search`, `/v2/map`, `/v2/crawl[/:id]`, `/v2/batch/scrape[/:id]` share the same
  internals. Formats accept strings or `{type}` objects; `maxAge` has no default; `sitemap`
  `skip|include|only` replaces `ignoreSitemap`/`sitemapOnly`; `maxDiscoveryDepth` replaces `maxDepth`.

### Later

LLM `json`/`extract` formats through an OpenAI-compatible endpoint with a spend cap; screenshots
from the browser tier; change tracking from stored captures; then the company-signal layers in
[BUILDOUT.md](BUILDOUT.md).
