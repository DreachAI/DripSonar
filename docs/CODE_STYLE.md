# Code conventions

Adapted from Dreach's engineering conventions; source revisions are in [DECISIONS.md](DECISIONS.md).
These rules apply when implementation begins. They are not claims that lint or build gates exist.

The target is **low handwritten LOC, high quality, and high modularity**. Prefer the smallest
readable implementation that satisfies the behavior and its failure cases. Reference checkouts
and generated/vendor code are not product LOC; keep them separate when reporting code size.

- A module owns one reason to change and exposes the smallest useful interface. Keep dependencies
  directional; avoid cycles, public internals, pass-through layers, and one-file-per-trivial-function
  fragmentation. Add a trait, crate, plugin system, or service only for a concrete consumer or boundary.
- Count complexity as well as lines: dependencies, concepts, public APIs, and operational components.
  Do not compress statements, hide behavior in clever macros, or remove validation/tests to lower LOC.
- Review quality through observable correctness, clear failure behavior, and maintainable boundaries.
  An unsupported numerical quality score is not evidence. Report net handwritten lines when useful,
  with the behavior or duplication that justified growth; no arbitrary LOC gate overrides correctness.
- Search for existing types and owners before writing. Keep modules named after their purpose;
  avoid catch-all `utils`, `helpers`, and speculative frameworks. Split at real ownership boundaries.
- Cull code the change makes obsolete, including its unused tests, config, and documentation.
  Before removing apparently dead code, check callers, dynamic loading, and public contracts.
  Preserve unrelated work; no speculative compatibility layers or commented-out implementations.
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
- Keep tests as short as possible while preserving meaningful assertions and readable setup.
  Prove each behavior once at the appropriate boundary; use table-driven cases where clearer.
  Avoid implementation mirrors, duplicate suites, mock-call trivia, and test-only abstraction layers.
- Do not spam gates. Reuse the existing check entry point; add a check only for a named failure
  it can catch. Run the smallest relevant checks, then broaden for concrete risk. Do not rerun
  unchanged suites or create a new script, review round, or checklist for every small edit.
- Measure before claiming performance. Compare the same source set, output requirements, hardware,
  cache state, and concurrency; include failure rates and extraction quality alongside latency.

When the first Rust package lands, provide one documented `tools/ci` entry point for the actual
format, lint, and relevant test commands. Do not add an empty gate during the documentation phase.

## Reviewer acceptance: 97/100+

Every reviewer grades the assigned scope on both correctness and fitness for the requested
outcome, including simplicity, modularity, code size, and effective tests. Each must reach 97/100
for ACCEPT. This is a reasoned reviewer assessment, not a measured accuracy or readiness metric.
State the scope, supporting evidence, deductions, and unverified limits; never invent a score
to satisfy the threshold. Unresolved correctness, security, evidence-integrity, or acceptance
defects require CHANGES_REQUIRED regardless of the total. Insufficient evidence cannot earn ACCEPT.

Below the bar, identify concrete gaps, repair them, and recheck the affected findings. Do not
repeat unchanged reviews or add ceremony to inflate confidence. A writer's self-score does not
replace an independent review; this rule governs reviews when used, not a mandatory review quota.
