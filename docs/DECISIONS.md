# Decisions

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
source or install its runtime as part of a reference update. Other projects are linked in research;
add a local checkout only when a concrete investigation needs one.

No DripSonar distribution license has been selected. Record the source and license for any code
copied or translated; Firecrawl's root AGPL license and per-directory exceptions must be considered
when selecting the port's code-reuse and distribution approach.
