# Structured JSON write compatibility and A/B observation — 2026-10-12

The reproduced failure was a correct JSON object in write_file.content being rejected by a string-only tool contract. The tool now explicitly advertises string/object/array input. Strings remain verbatim; objects/arrays serialize only for .json targets, case-insensitively. Invalid types and structured non-JSON targets fail before mutation. Path resolution, filesystem providers and approval/safety dispatch remain unchanged.

Fixture version 3 allows either a complete JSON object or a serialized string, matching the new tool contract. This differs from previous version-2 instructions, so the runs are not pooled. The model and endpoint were unchanged: GLM 5.3 Flash on user-selected local GPT-Load. Binary fingerprint and observations are in [the JSON report](2026-10-12-context-json-compatible.json).

| Observation | Compaction off | Compaction on |
| --- | ---: | ---: |
| Successful file/workflow | 1/1 | 1/1 |
| Model requests | 4 | 3 |
| Prompt tokens | 52,885 | 27,180 |
| Completion tokens | 722 | 391 |
| Reported cached prompt tokens | 27,520 | 17,920 |
| Prompt tokens outside reported cache hits | 25,365 | 9,260 |
| Reported cache hit rate | 52.04% | 65.93% |
| Usage completeness | Complete | Complete |

Both runs used read_file and then write_file with an object-valued content argument, and both produced the correct JSON artifact while preserving unrelated nested/Unicode fields, protected.txt and the remaining workspace. The off run additionally re-read the result and reached the three-step limit, triggering a final-summary request. The on run finished after two tool calls and a final answer. All these requests remain in the accounting.

Total input was 48.61% lower and observed input+output was 48.57% lower for the compacted run. This includes a difference in request count and stochastic model behavior; it cannot be attributed solely to compaction. Both policies succeeded on one task each, which is not quality non-inferiority evidence. Order was off then on and provider cache state remained uncontrolled.

Under hypothetical constant per-million rates Ru (uncached input), Rc (cached input), and Ro (output), compacted-minus-uncompacted token charge would be `(-16105 * Ru - 9600 * Rc - 331 * Ro) / 1000000`. These observed token counts would favor compaction for positive rates in this one run, but actual invoices, gateway/upstream retry charges and cache-write charges were not measured. No guaranteed monetary saving is claimed.

Validation: 349 default Rust tests, 347 containment tests, 15 evaluation tests (including streamed object-write real CLI simulation), 8 usage-report tests, fmt, warnings-denied clippy and release build passed. Independent read-only review found no concrete regression. Raw sessions remain at `/private/tmp/sigil-context-json-compatible-lb0cwy20/run/`; authentication was transient and no credential or raw log is committed.
