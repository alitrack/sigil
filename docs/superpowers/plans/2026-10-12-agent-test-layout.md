# Agent test layout

Move the inline `parallel_tests` and `compact_tests` modules from
`src/agent/mod.rs` into sibling files, retaining their module names and
parent scope. This reduces the main loop file from 3,723 to 1,218 lines.
Runtime source is unchanged; test bodies change only through dedenting
and rustfmt formatting.

Validation completed:

- Exact test names are unchanged in both build modes.
- Default: 349 tests passed; without default features: 347 passed.
- Formatting, clippy with warnings denied, and release build passed.
- Independent read-only review found no issues.
- Release size remains 10,012,512 bytes. The binary hash changes after
  rebuilding; this is source organization, not a binary-size reduction.

No model requests or billing assumptions are involved.
