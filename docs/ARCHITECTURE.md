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
| `page` | Pure HTML transforms: cleanup, main content, markdown, fit markdown, metadata, links | `render(&Fetched, &RenderOptions) -> Rendered` | `extract::css` (pure) |
| `store` | SQLite metadata and content-addressed blob files; capture cache | `Store::{cached, put}` | net (types only) |
| `scrape` | One scrape: cache, single-flight, fetch, classify, render | `Scraper::scrape(&ScrapeRequest) -> Result<Document, ScrapeError>` | net, page, store |
| `search` | Search providers and optional per-result scraping | `Searcher::search(&SearchRequest) -> Result<Vec<Hit>, SearchError>` | net, scrape |
| `api` | HTTP routes, both wire shapes, validation, auth, error mapping | `router(AppState) -> axum::Router` | scrape, search |
| `main` | CLI and server lifecycle | — | api, config |

Dependencies are acyclic in that order. `page` never touches the network or disk. Page collection
uses `net::fetch` and its destination guard; structured extraction uses a separate operator-trusted
endpoint (see Structured extraction below).

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

Rendering runs off the async runtime on blocking threads, bounded by `SONAR_RENDER_THREADS`
(also used for browser session views); the deadline still applies while waiting for a slot.
Interactive scrapes (v1/v2 scrape, context scrape, search with scrape) return 507
`SCRAPE_STORAGE_FULL` below the free-disk floor (`SONAR_MIN_FREE_DISK_MB`), checked once per request.

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
| Capture could not be stored | `SCRAPE_STORAGE_ERROR` | 500 |
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
- v1/v2 `tbs` accepts `qdr:h|d|w|m|y`: SearXNG receives `time_range`, DuckDuckGo
  receives `df` (hour requests use the providers' minimum day window). Filtered requests never
  fall through to providers that cannot honor them. v2 `sources: ["news"]` uses SearXNG's news
  category and returns `data.news`; without `SEARXNG_ENDPOINT` it is a named 400. Combining web
  and news in one request is not supported.

## Storage (`store`)

- `<data_dir>/sonar.db` (SQLite, WAL, foreign keys) with a `captures` table: id, cache key,
  requested URL, final URL, status, selected headers, content type, blob hash, byte length,
  fetched_at. `<data_dir>/blobs/sha256/<ab>/<hash>` holds raw bodies.
- The database is the cache index and the audit log. Captures are appended, never updated.
- Capture retention is 30 days: an hourly sweep deletes older captures (their `searches` rows
  cascade) and blobs nothing references.
- A `searches` table records each stored search (query, provider, language, time, hits) against
  its capture row (`capture_id`, cascading delete); `sonar export --kind search` emits it.

## API (`api`)

- Routes: `/v1/scrape`, `/v1/search`, `/v1/web/scrape`, `/v1/web/search`, `GET /health`, and the
  browser session and artifact routes (Browser tier below).
- Request bodies deny unknown fields and are capped at 1 MiB. Fields Firecrawl accepts but this
  engine does not implement (`extract` format, `location`) are rejected with `BAD_REQUEST`
  naming the field rather than silently ignored; `mobile`, `blockAds`, and `storeInCache`
  are accepted as no-ops. `actions`, screenshot formats, and `waitFor` use the browser tier.
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
  and never included in status or export records. Full-URL regexes are rejected by
  name; `maxConcurrency` and `deduplicateSimilarURLs` are accepted no-ops. Batch scrape options
  are top-level. New jobs are refused at the active-job ceiling or below the free-disk floor
  (`SONAR_MIN_FREE_DISK_MB`, default 2048; HTTP 507). Transient job-page failures are retried
  after 5 s and 30 s (3 attempts in all).
- Crawl and batch `webhook` accepts a URL or `{url, headers?, metadata?, events?}`. Started,
  page, completed, and failed events commit to the `webhooks` outbox with their job transition;
  bodies use the shared API document serializer and Firecrawl's `crawl.*` / `batch_scrape.*`
  envelopes. Four deliveries run at once through `Fetcher::post_json`: the same destination
  guard, DNS pinning, gate and proxy budget, no redirects, and a ten-second deadline. Each event
  gets at most three attempts, backing off five and sixty seconds; restart resumes pending work.
  Delivery is not exactly-once: receivers must tolerate duplicates. `SONAR_WEBHOOK_SECRET`
  adds `X-Sonar-Signature: sha256=<HMAC-SHA256 hex>` over the exact JSON bytes. Custom transport
  and signature headers are refused. Webhook credentials are removed after terminal delivery
  or exhaustion; expired jobs with pending attempts are retained until attempts finish.
- `GET /v1/crawl/active` and `/v2/crawl/active` return active crawl IDs, seed URLs, and an explicit
  public option subset; neither scrape headers nor webhook credentials are exposed.
- `sonar export [--kind pages|search|all] [--since RFC3339] [--host SUFFIX] [--out PATH]` (default
  `pages`; `all` emits pages then searches) streams captures as JSON Lines,
  using the shared capture renderer, filtering failed/blocked/unsupported/empty pages, and
  deduplicating markdown hashes. Records include capture and blob IDs, source dates as stated,
  route, and `tier: "http"`. Existing output files are never overwritten. The deduplication set
  grows with unique exported documents; export does not read job specifications or headers.

### Browser tier (`browser`)

As built (2026-10-22 browser council, DECISIONS.md). Crate `chromiumoxide` 0.9; no chromiumoxide
type leaves `browser`.

- **Chrome:** one supervised Chrome. `SONAR_CHROME_WS` connects to an external Chrome confined by
  `deploy/sonar-chrome.service` (loopback-only IP filter; production). `SONAR_CHROME_PATH` launches a
  child Chrome with a fresh `--user-data-dir` (development; no OS confinement). Neither set: the tier
  is off and actions, screenshots, `waitFor > 0`, and sessions fail with
  `SCRAPE_ALL_ENGINES_FAILED` ("browser tier not configured"). Chrome starts on first use.
- **Egress:** Chrome's only route is the guarded forward proxy (`egress`): `--proxy-server` to it
  (fixed `SONAR_FORWARD_PROXY_PORT`, default 3003, for the external unit; a random port for a child),
  `--proxy-bypass-list=<-loopback>`, `--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1`,
  `--disable-quic`, WebRTC `disable_non_proxied_udp`. `Fetch` interception runs on the browser
  session, so it alone answers auth: our proxy's challenge gets the per-process token once per
  request; site challenges are cancelled. The top-level URL first passes the guard, DNS policy, and
  the per-host gate (held for the visit); subresources take no gate slot.
- **Per visit:** a fresh browser context (disposed on detach) and one tab, 1280x800, declared user
  agent set per tab, downloads denied, popups closed. Caller headers go only to the requested
  origin's main-frame requests. Text-only visits block images, fonts, and media. Top-level redirects
  are capped at 5 and HTTPS-to-HTTP is refused. Navigation waits for load (at most 3 s) plus 500 ms;
  then `waitFor`, actions, the format screenshot, and the final DOM, in Firecrawl's order. The
  status is the document's real navigation status; unknown fails the visit.
- **Limits:** 4 tabs (`SONAR_BROWSER_TABS`), 16 queued (then `CONCURRENCY_QUEUE_TIMEOUT`), queueing
  counts against the deadline; 10 s per DevTools command (navigation: the request deadline), 2 s
  cleanup or the instance is failed. Retire after 200 visits (`SONAR_BROWSER_RECYCLE_PAGES`): drain
  one-shot visits, ask Chrome to exit (systemd restarts the external one). A child's process tree
  over `SONAR_BROWSER_MEMORY_MB` (Linux `/proc`, sampled each second) or a lost connection kills the
  instance; 3 failures a minute cool down for 30 s. Actions are never replayed.
- **Escalation (`policy`):** plain HTTP first. Escalate only a 2xx HTML page with fewer than 200
  visible characters and a concrete shell marker (empty app root plus external or module scripts, or
  a JavaScript-required notice); never challenges or error statuses. A render is used only if it
  shows at least 200 visible characters and twice the HTTP text; then its origin goes browser-first
  for 10 minutes (at most 4,096 origins); a failed render forgets it and falls back to HTTP.
- **Cache:** rendered captures live in their own key namespace (URL, headers, user agent, viewport,
  asset policy, route, policy version) and are stored with `tier = 'browser'` (column added in place;
  older rows are HTTP) as UTF-8 HTML. Automatic renders honor `maxAge` and share identical concurrent
  visits; explicit visits skip cache reads and sharing, and those with actions are stored under a key
  no lookup produces.
- **Actions:** Firecrawl v1 `wait`, `click`, `write`, `press`, `scroll`, `screenshot`, `scrape`,
  `executeJavascript`; `pdf` is refused. Validated before fetching: 50 actions, 60 s total wait
  (selector waits count 1 s), selectors 2 KiB, text and scripts 64 KiB, known keys and modifiers.
  An action that starts a navigation waits for the new document. Results:
  `actions: {screenshots, scrapes, javascriptReturns, pdfs: []}`; a failure is
  `SCRAPE_ACTION_ERROR` naming the action's index and type, never logging its arguments.
- **Screenshots:** formats `screenshot` and `screenshot@fullPage` (above 16 megapixels fails), PNG or
  JPEG with `quality`, 8 MiB each and 32 MiB per request. Stored through the blob writer under
  `blobs/artifacts` with an `artifacts` row (random 256-bit ID, hash, media type, size, 24 h
  expiry; 1 GiB total). Returned as `SONAR_PUBLIC_URL/v1/artifacts/<id>` (default
  `http://<bind>`), served without the API key with `nosniff`; a sweeper deletes expired ones.
- **Sessions:** `POST /v1/browser {url, timeout?, formats?}` → `{success, id, url, markdown,
  screenshot?}`; `POST /v1/browser/:id {actions, formats?, timeout?}` → `{success, url, markdown,
  screenshot?, actions}`; `DELETE /v1/browser/:id`. One tab each, steps serialized, at most
  `SONAR_BROWSER_SESSIONS` (default 2, capped at tabs − 1), 120 s idle, 15 min lifetime, reaped every
  10 s; a Chrome restart ends them.

### Egress and proxy route (`egress`)

Blocked GET pages (challenge interstitials or status 401/403/429) recover once through SOCKS5,
then once through the browser if still blocked, under the original deadline. `proxy: auto`
(default) enables this; `basic` skips proxy recovery, while `stealth`/`enhanced` pins the proxy
route from the start and disables recovery. `SONAR_BLOCK_RECOVERY=false` disables automatic
block recovery. Proxy recovery ignores `SONAR_PROXY_HOSTS`; browser recovery keeps that override
in its isolated context through the guarded forward proxy. Route-specific caches stay separate;
returned metadata records attempted stages in `recovery` and the final capture's `proxyUsed`.
Host 429/503 responses or `Retry-After` double shared network spacing up to 30 seconds for ten
minutes (never below configured spacing); successful 2xx responses halve it toward the baseline.

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

- As built: `/v2/scrape`, `/v2/search`, `/v2/map`, `/v2/crawl[/:id]`, and
  `/v2/batch/scrape[/:id]`, including job DELETE and `/errors`, reuse v1 collection and job owners.
  Status and submission URLs stay on v2. Search returns `data.web`; map returns `links: [{url}]`.
- Formats accept strings or strict `{type}` objects: markdown, html, rawHtml, links, screenshot.
  Screenshot `fullPage` and JPEG `quality` map to the browser; `viewport` is rejected (fixed viewport).
  `parsers: ["pdf"]` or `[{type:"pdf"}]` enables PDF parsing; `[]` disables it. Absent `maxAge`
  uses server configuration. `sitemap: skip|include|only` maps to shared discovery, with `only`
  disabling page-link discovery; crawl uses `maxDiscoveryDepth`, `crawlEntireDomain`, `allowSubdomains`.
- Search defaults to 10 hits, accepts web or news sources (strings or `{type}`), and applies
  includeDomains or excludeDomains to returned hosts/subdomains before optional scraping; no refill.
  `location`, source options, and other unsupported fields/formats/parsers get named 400s.
  Shared v1 no-ops remain no-ops. Jobs reject screenshots, nonempty actions, and positive waitFor
  because workers do not execute browser plans. Legacy v1-only option names are rejected.

### Structured extraction (`extract`)

- As built: v1 `json` with `jsonOptions: {schema?, prompt?, systemPrompt?}` and `summary`;
  v2 `{type:"json", schema?, prompt?}` and `{type:"summary"}`. Scrape and search share extraction;
  crawl and batch explicitly reject these formats. Without `SONAR_LLM_URL`, requests get a named 400.
- `SONAR_LLM_URL` is an operator-trusted OpenAI-compatible base URL, including private/loopback
  endpoints; `SONAR_LLM_MODEL` is required and `SONAR_LLM_API_KEY` is optional and redacted.
  A dedicated client disables proxies and redirects, bypasses the page guard, and caps replies at
  4 MiB. No page text, prompts, or provider keys are logged.
- Page markdown is generated even when not returned. `SONAR_LLM_MAX_INPUT_CHARS` (60000) truncates
  at a character boundary with a warning; randomized delimiters and system instructions mark page
  text as untrusted data. Temperature is zero; `SONAR_LLM_MAX_OUTPUT_TOKENS` defaults to 2048.
- Schema requests use `json_schema`, falling back to `json_object` on HTTP 400/422. Local validation
  supports primitive/nullable types, required keys, nested properties/items, boolean schemas,
  and `additionalProperties` booleans; unsupported constraints are rejected before collection.
  Invalid JSON or schema mismatch retries once with the validation error, then returns `json:null`
  and a warning. Summary uses a validated string field. Failures never fail a successful page scrape.
- `SONAR_LLM_CONCURRENCY` (4) bounds shared provider work; `SONAR_LLM_TIMEOUT_MS` (60000) covers
  queueing, fallback, and retry for each format. Cancellation drops the request and releases its slot.
  Reported token counts accumulate across attempts and formats in `metadata.llmUsage` without
  inventing missing counts. Extraction is per caller, not cached or persisted. No monetary spend cap
  is implemented; operator enablement and request/token/concurrency limits bound individual work.

### Fit markdown (`page::fit`)

- As built: DripSonar extension format `fitMarkdown` (v1 string, v2 `{type:"fitMarkdown"}`),
  returned as `data.fitMarkdown`; jobs accept it. Runs in the bounded blocking render path.
- `dom_smoothie` 0.18 readability (capped at 100k elements) runs on the cleaned body (after
  include/exclude tags, before main-content selectors) when the page DOM is within 200k elements
  and 256 levels; past that, or under 200 visible characters, the cleaned body stands in. Pruning
  then drops boilerplate tags and comments, removes any element whose class or id has a whole
  token (split on non-alphanumerics) `nav`, `footer`, `header`, `sidebar`, `ads`, `ad`, `comment`,
  `comments`, `promo`, `advert`, `social`, `share`, or `sharing`, except `main`, `article`,
  `[role=main]` and their ancestors, and walks top-down removing subtrees scoring under 0.48 on
  0.4·text density + 0.2·(1 − direct-link text share) + 0.2·tag weight + 0.1·ln(text length + 1)
  (Crawl4AI's pruning filter, reimplemented; lengths in characters, markup counted escaped).
  Images and other void elements are not scored; they go with their container, and textless
  elements holding a linked image are never pruned for their score. When pruning keeps under a
  fifth of the visible text, the unpruned article (or body) is rendered instead.
- Links become `text⟨N⟩` and images `![alt⟨N⟩]`, numbering unique http(s) URLs in first-seen
  order, followed by `## References` lines `⟨N⟩ url: title` (title attribute, else link text),
  whitespace-collapsed with `` \ [ ] * _ ` < > ⟨ ⟩ `` backslash-escaped.
- Readability feeds only `fitMarkdown`; `markdown` keeps the main-content selector path. Plain
  text and PDFs return their markdown unchanged as `fitMarkdown`.

### CSS extraction (`extract::css`)

- As built: zero-spend structured extraction through v1 `jsonOptions.selectors` or v2
  `{type:"json", selectors}` with a Crawl4AI-like schema `{baseSelector?, fields:[{name,
  selector?, type: text|attribute|html|list|nested, attribute?, fields?}]}`. A field without
  `selector` reads the element in scope. `data.json` is an array (one object per `baseSelector`
  match) or one object for the page. Text is whitespace-collapsed; missing matches are `null`.
- Validated before collection (valid selectors, unique names, at most 100 fields, nesting depth
  4); runs on the original DOM in the render path. A DOM over 200k elements or 256 levels, a
  2M-node work budget (one per node walked), or an 8 MiB output budget (keys and values, charged
  before copying) makes `json` `null` with a warning; selections stop at 1000 matches. Selectors
  win over an LLM schema in the same request, so no LLM call is made, and work without
  `SONAR_LLM_URL`. Jobs still reject the json format.


### Change tracking

- v1 `changeTracking` with optional `changeTrackingOptions: {modes: ["git-diff"]}` and v2
  `{type: "changeTracking", modes?: ["git-diff"]}` require markdown. Both use the newest prior
  successful stored capture matching the normalized requested or final URL, strictly before the
  current capture's time and ID; failed pages and search exchanges never become the baseline.
- The baseline is rendered with the current options. Exact markdown equality yields `same`,
  differences `changed`, no baseline `new`, and a current 404/410 with a baseline `removed`.
  `previousScrapeAt` is the baseline capture time (null for new), visibility is `visible`, and
  requested git diffs use `similar`'s unified line diff on bounded rendering workers. Damaged
  history fails instead of inventing a new page. Jobs persist comparison results with documents.

The company-signal layers remain later work in [BUILDOUT.md](BUILDOUT.md).
