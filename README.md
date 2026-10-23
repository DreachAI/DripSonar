# DripSonar

A Rust web research and collection engine, planned around Firecrawl capabilities and selected
`drip-kindness` collection methods. First use case: **company research and specific business signals**.

Given a company and a research question, the intended output is relevant evidence and structured
signals with source URLs, dates, entity matches, and a clear separation of facts from inference.

## Status

Bootstrap and reference research only. Agent guidance, a staged buildout, and an upstream source
checkout are present. The Rust application and script integration have not started; nothing is
deployed or benchmarked.

## Repository map

| Path | Purpose |
| --- | --- |
| [AGENTS.md](AGENTS.md) | Instructions for agents; `CLAUDE.md` imports the same file |
| [.agents/](.agents/README.md) | Shared role questions and runbooks; native bindings in `.codex/` and `.claude/` |
| [docs/](docs/README.md) | Buildout, code conventions, decisions, and tool research |
| [references/](references/README.md) | Pinned upstream source for inspection |

## Check out the reference

```bash
git clone https://github.com/DreachAI/DripSonar.git
cd DripSonar
git submodule update --init --depth 1
```

For an existing checkout:

```bash
git submodule update --init --depth 1
git submodule status
```

Start with [the buildout](docs/BUILDOUT.md) and [the tool comparison](docs/RESEARCH.md).
