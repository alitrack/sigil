# Context policy A/B observation — 2026-10-12

Endpoint: user-selected local GPT-Load at `http://127.0.0.1:3001/v1`. Model: `z-ai/glm-5.3-flash`. Authentication was used transiently; no credential is stored in this report. Exact binary identity and per-request observations are in [the JSON report](2026-10-12-context-ab.json).

Two paired repetitions used identical synthetic histories within each pair. Policy order was off/on, then on/off. Off disabled proactive compaction; on used an 8,000-token threshold. No tool calls were requested by the fixture. Cache state was uncontrolled.

| Observation | Compaction off | Compaction on |
| --- | ---: | ---: |
| Strict-answer success | 1/2 | 1/2 |
| Observed model requests | 2 | 2 |
| Prompt tokens | 27,798 | 17,652 |
| Completion tokens | 106 | 141 |
| Reported cache hit rate | 0% | 0% |
| Cache reporting coverage | 2/2 requests | 2/2 requests |
| Usage completeness | Complete | Complete |

The compacted policy used 36.50% fewer input tokens in this sample, with 35 more output tokens. This is an observation for one synthetic task-key retention fixture, not a billing measurement or a general coding-quality result. Both first-pair responses failed the exact-answer criterion; both second-pair responses passed. At least one failure omitted the literal `key-` prefix, so the strict score includes formatting compliance as well as retention. The small sample provides no quality non-inferiority evidence.

Provider prices, internal gateway/upstream retry billing, cache-write charges, and actual invoice totals were not observed. Zero reported cache hits apply only to these four requests; they do not establish behavior in repeated warm-cache sessions. Do not generalize the token reduction into guaranteed savings.

Initial attempts using `deepseek-ai/deepseek-v4-flash-0731` failed before reported usage. A minimal diagnostic request returned HTTP 410 stating that model was no longer available. Those attempts are not included in this GLM result; their cost is unknown rather than zero. Their separate raw observation report remains at `/private/tmp/sigil-context-eval-real-ld6ka3dx/run/report.json`. GLM sessions remain at `/private/tmp/sigil-context-eval-glm-vk62jxmx/run/` and are not committed.

The first real comparison is complete. Broader coding tasks, more repetitions, controlled cache experiments where the provider permits them, and provider billing reconciliation remain necessary before choosing a production policy on cost/quality grounds.
