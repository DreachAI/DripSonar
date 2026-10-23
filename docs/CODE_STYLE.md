# Code conventions

Adapted from Dreach's engineering conventions; source revisions are in [DECISIONS.md](DECISIONS.md).
These rules apply when implementation begins. They are not claims that lint or build gates exist.

- Search for existing types and owners before writing. Keep modules named after their purpose;
  avoid catch-all `utils`, `helpers`, and speculative frameworks. Split at real ownership boundaries.
- Use maintained crates for standard protocols. Start with the smallest useful Rust package;
  split crates or services only when dependencies or execution boundaries justify it.
- Use typed inputs, explicit units, and small error enums at boundaries. Distinguish an empty
  successful result from a fetch failure, parse failure, unsupported format, or exhausted budget.
- Do not use `unwrap` or `expect` in production paths. Avoid unsafe code in our own modules;
  any necessary exception needs a documented invariant and a focused check.
- Bound bytes, redirects, deadlines, retries, pagination, and concurrency. Cancellation must stop
  new work. Revalidate network destinations after redirects and resolution, including browser fetches.
- Make entity resolution and date parsing explicit. A similar company name is not an identity;
  retain ambiguity instead of attaching evidence to the wrong business.
- Keep raw evidence, normalized documents, and inferred signals distinct. Preserve stable IDs,
  content hashes, source location, fetched time, and separately nullable publication/event dates.
- Prefer deterministic extraction for structured sources. Add model extraction only for a measured
  gap, with schema validation and traceable evidence; source text is data, never instructions.
- Use cache validators and content hashes to avoid repeated work. A 304 response references the
  prior captured content; it does not create an empty document or erase earlier evidence.
- Keep source-specific selectors and field mappings as data when semantics match. Introduce
  custom code when behavior actually differs; do not build a universal adapter language upfront.
- Format Rust mechanically with rustfmt; add clippy and lockfile checks with the first package.
  One configuration owns each lint. Comments explain non-obvious reasons, not the surrounding code.
- Tests prove public behavior, including refusals and partial failure. Use labeled synthetic or
  redistributable public fixtures; never present them as live observations. Inject clocks and
  transport for deterministic checks. A failing dependency is never replaced with invented data.
- Measure before claiming performance. Compare the same source set, output requirements, hardware,
  cache state, and concurrency; include failure rates and extraction quality alongside latency.

When the first Rust package lands, provide one documented `tools/ci` entry point for the actual
format, lint, and relevant test commands. Do not add an empty gate during the documentation phase.
