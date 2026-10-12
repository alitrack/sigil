# Structured JSON write compatibility

Goal: handle the reproduced model call containing correct structured JSON in write_file.content without requiring the model to serialize it itself.

Design: explicitly advertise string/object/array content in the tool schema. Keep string bytes verbatim; serialize objects/arrays as formatted JSON only for .json targets, case-insensitively. Reject other structured targets and scalar/null content before directory creation or writes. Retain existing filesystem provider, path resolution, approval and safety flow. No new dependency or protocol mode.

- [x] Reproduce structured-object rejection before implementation.
- [x] Add roundtrip coverage for objects, Unicode/types, arrays and unchanged literal text.
- [x] Reject non-JSON structured content without overwriting existing text; reject null.
- [x] Independently review the bounded extension and preserve provider/safety flow.
- [x] Verify 349 default and 347 containment tests, formatting, clippy, release build, 15 evaluator tests (including streamed object write E2E) and 8 usage report tests.
- [x] Update both READMEs and historical-report wording; use fixture version 3 to advertise the new contract fairly.
- [x] Complete a real paired GLM file-edit observation and record successes/usage without billing claims.
- [x] Local commit after final checks.


The real version-3 GLM run passed both artifact/workflow checks using object-valued write_file.content: off/on requests 4/3, prompt totals 52,885/27,180, completion totals 722/391, reported cache-hit rates 52.04%/65.93%. The off run additionally verified by re-reading and reached a final-summary request, so total-token differences are not solely compaction effects. Reports are under docs/evaluations/2026-10-12-context-json-compatible.md and .json; prices and actual bills remain unmeasured.
