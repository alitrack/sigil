# Context JSON-edit A/B observation — 2026-10-12

Model: `z-ai/glm-5.3-flash` via user-selected local GPT-Load. Each run used one paired repetition, off then on. The target JSON and historical constraints matched within the pair; off disabled proactive compaction, on used an 8,000-token trigger. Cache state was uncontrolled. The fixture required read_file/write_file, three field updates, preservation of unrelated nested/Unicode data, an unchanged protected file, and no unexpected workspace changes. The checker did not execute generated code.

Two complete runs are retained: [initial instructions](2026-10-12-context-json-initial.json) and [explicit string parameter instructions, fixture version 2](2026-10-12-context-json-explicit-string.json). The latter explicitly reminded the model that write_file.content must be a serialized string. It is a changed prompt, so the two runs are separate observations rather than a pooled comparison.

| Observation, explicit-string run | Compaction off | Compaction on |
| --- | ---: | ---: |
| Successful artifact/workflow | 0/1 | 0/1 |
| Reported model requests | 4 | 4 |
| Prompt tokens | 52,421 | 32,261 |
| Completion tokens | 352 | 761 |
| Reported cached prompt tokens | 26,880 | 17,280 |
| Prompt tokens outside reported cache hits | 25,541 | 14,981 |
| Reported cache hit rate | 51.28% | 53.56% |
| Usage completeness | Complete | Complete |

Compaction used 38.46% fewer input tokens in this run, but both tasks failed. write_file.content was repeatedly a JSON object, while the current tool contract requires a string. The registry rejected the type; neither settings file changed. Comparison of the structured payloads with the desired JSON confirmed the desired fields were correct for all write attempts. The off run additionally requested prohibited bash and therefore failed workflow validation as well. This fixture exposes an interface-compliance failure; it does not show that compression damaged the historical JSON constraints.

Each run used three tool-loop steps followed by a final-summary request at the step limit. The four reported requests per policy include that wrap-up. Failed attempts and wrap-up usage remain in the totals; observed tokens per successful task is null because there were no successes.

Reported cache hits were present in this file-edit experiment, unlike the earlier four-request key-retention sample. The cached and non-hit input counts both decreased in the compacted group, while output increased by 409 tokens. If a provider billed only these observed tokens at constant per-million rates Ru (uncached input), Rc (cached input), and Ro (output), the hypothetical compacted-minus-uncompacted charge would be `(-10560 * Ru - 9600 * Rc + 409 * Ro) / 1000000`. Prices, invoices, cache-write fees and hidden upstream billing were not observed, so this is an arithmetic relation rather than measured monetary savings. Failed-task cost cannot establish production efficiency.

A single off/on pair has order and stochastic effects; no quality non-inferiority or controlled cold/warm-cache conclusion follows. Raw sessions remain local at `/private/tmp/sigil-context-json-real-wdbjnhmk/run/` and `/private/tmp/sigil-context-json-v2-real-xp5bkq_x/run/`; neither raw logs nor credentials are committed. These results provide a reproducible tool-parameter compatibility case for subsequent work.
