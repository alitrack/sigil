# JSON edit context evaluation plan

Goal: extend the existing paired context evaluator with a deterministic JSON file-edit task, testing tool use and historical constraints without executing generated code.

Architecture: keep the retention fixture as the default; add --case json-edit. Both policies receive identical seeded histories and settings.json files. The historical user instruction specifies three changed service fields, while unrelated nested fields and a Unicode value must remain unchanged. Validate the final file structurally and require successful process exit; stdout cannot substitute for a correct artifact. The validator rejects symlinked, oversized, malformed and duplicate-key JSON, and checks a separate protected file. Credentials remain transient, and summaries retain the existing conservative usage accounting.

- [x] Write failing regressions for identical seeds, artifact correctness, preservation, invalid JSON and process failures.
- [x] Implement fixture preparation, per-case prompt and structural validation; add CLI case selection.
- [x] Verify offline tests and local real-CLI simulation with streamed read_file/write_file calls.
- [x] Independent review and bilingual documentation.
- [x] Run one paired real GLM experiment using the previously authorized 3001 service; record observed usage and artifact outcomes without currency claims.
- [x] Save non-sensitive report and local commit after checks pass.


Validation: 15 evaluation tests, including streamed real-CLI read_file/write_file E2E, and 8 usage-report tests passed. Independent review identified missing tool workflow/unexpected-file validation; reproduced and fixed. A runtime-created empty harness/skills directory initially caused a snapshot mismatch; it is now prepared before snapshots. The final checker uses bounded regular-file reads and bounded workspace traversal.

Real runs: two separate one-pair GLM observations were completed, before and after clarifying the string content parameter. Both policies failed artifact success because GLM returned correct structured JSON as an object instead of the string required by write_file. The version-2 off run also used prohibited bash. The complete observed metadata and arithmetic limitations are saved under docs/evaluations/2026-10-12-context-json*.md/.json. The checker did not execute generated code; no credential was written to a deliverable. Success per task was zero, so no useful-cost benefit is asserted.
