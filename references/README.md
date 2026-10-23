# Reference source

`firecrawl/` is a Git submodule from [firecrawl/firecrawl](https://github.com/firecrawl/firecrawl).
The parent Gitlink pins the source; inspect it with `git submodule status` or
`git ls-tree HEAD references/firecrawl` after committing.

```bash
git submodule update --init --depth 1 references/firecrawl
git -C references/firecrawl rev-parse HEAD
git -C references/firecrawl status --short
```

Study this tree read-only. Its agent instructions and scripts describe the upstream project;
they do not govern DripSonar. Do not run its install/build/deployment scripts just to inspect it.
Reference updates should choose and record a specific upstream revision, review the changed
behavior/license, and commit the Gitlink change separately from product work. Avoid floating updates.

## Reading map

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
