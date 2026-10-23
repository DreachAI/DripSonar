# DripSonar

A self-hosted web collector in Rust that AI agents use the way a person uses the web: search,
read pages (including script-rendered sites and PDFs), click and type through pages, crawl sites,
and keep everything it collected for later training. It speaks Firecrawl's v1 and v2 APIs and
context.dev's search/scrape shapes, so existing clients can point at it unchanged.

## Status

Built: scrape, search, map, crawl, batch scrape, browser actions and sessions, screenshots, PDF
text, residential proxy routing, JSON Lines export. One binary, `sonar`, plus an optional headless
Chrome. Not yet deployed or benchmarked; owner policy during the build-out is no tests or CI.
Design: [ARCHITECTURE.md](docs/ARCHITECTURE.md). Why: [DECISIONS.md](docs/DECISIONS.md).
Work tracking: Linear team DripSonar (SNR).

## Run

```bash
cargo build --release
./target/release/sonar serve                    # 127.0.0.1:3002, data in ./data
./target/release/sonar scrape https://example.com
./target/release/sonar search "query"
./target/release/sonar export --kind all --out dataset.jsonl
```

Settings are flags or `SONAR_*` environment variables; `sonar --help` lists them and
[deploy/sonar.env.example](deploy/sonar.env.example) shows the common ones. Set
`SONAR_CHROME_PATH` (or `SONAR_CHROME_WS`) to enable the browser tier, `SONAR_PROXY=socks5://…`
with `SONAR_PROXY_HOSTS` for residential routing, and `SEARXNG_ENDPOINT` for a self-hosted
search backend. EC2 deployment: [deploy/README.md](deploy/README.md) and [scripts/](scripts/README.md).

## Endpoints

| Area | Routes |
| --- | --- |
| Firecrawl v1 | `/v1/scrape`, `/v1/search`, `/v1/map`, `/v1/crawl[/:id[/errors]]`, `/v1/batch/scrape[/:id[/errors]]` |
| Firecrawl v2 | the same under `/v2` |
| context.dev | `/v1/web/scrape`, `/v1/web/search` |
| Browser sessions | `POST /v1/browser`, `POST`/`DELETE /v1/browser/:id`; screenshots at `/v1/artifacts/:id` |
| MCP (agent tools) | `POST /mcp` (Streamable HTTP, stateless; `GET` answers 405); `sonar mcp` serves the same tools on stdio |
| Health | `GET /health` |

## Use from agents

The MCP server gives agents the tools `search`, `scrape`, `extract`, `map`, `crawl_start`,
`crawl_status`, `crawl_cancel`, `browser_open`, `browser_step`, and `browser_close`. Results are
capped at `SONAR_MCP_MAX_CHARS` (default 40000) with a note when cut.

Claude Code (add `--header "Authorization: Bearer $SONAR_API_KEY"` when a key is set):

```bash
claude mcp add --transport http dripsonar http://127.0.0.1:3002/mcp
claude mcp add dripsonar-local -- sonar mcp    # stdio, no server needed
```

OpenCode (`opencode.json`):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "dripsonar": {
      "type": "remote",
      "url": "http://127.0.0.1:3002/mcp",
      "headers": { "Authorization": "Bearer {env:SONAR_API_KEY}" }
    },
    "dripsonar-local": {
      "type": "local",
      "command": ["sonar", "mcp"],
      "environment": { "SONAR_DATA_DIR": "/path/to/data" }
    }
  }
}
```

Drop `headers` when no key is set. A stdio server sharing a data directory with a running
`sonar serve` works, but its crawl tools report unavailable (the server owns the job lock).

## Repository map

| Path | Purpose |
| --- | --- |
| [AGENTS.md](AGENTS.md) | Instructions for agents; `CLAUDE.md` imports the same file |
| [src/](src/) | The `sonar` library and binary |
| [docs/](docs/README.md) | Architecture, decisions, research, conventions |
| [deploy/](deploy/README.md), [scripts/](scripts/README.md) | EC2 units and operational scripts |
| [references/](references/README.md) | Pinned upstream source for inspection (`git submodule update --init --depth 1`) |
