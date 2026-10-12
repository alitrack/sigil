# Alternating-order JSON edit replication — 2026-10-12

Repeated fixture version 3 on the same GLM 5.3 Flash endpoint and exact binary fingerprint as the previous successful compatibility run. Two paired repetitions alternated off/on and on/off. Histories and target files matched within each pair; task keys and requested ports varied between pairs. Authentication remained transient. [Full reported observations](2026-10-12-context-json-repeat.json).

All four runs passed final artifact and recorded workflow checks. Unrelated nested/Unicode content, protected.txt and the remaining workspace were preserved.

| Observation | Compaction off | Compaction on |
| --- | ---: | ---: |
| Successful tasks | 2/2 | 2/2 |
| Reported model requests | 7 | 6 |
| Prompt tokens | 95,118 | 54,366 |
| Completion tokens | 777 | 1,129 |
| Reported cached input | 55,040 | 35,840 |
| Input outside reported cache hits | 40,078 | 18,526 |
| Weighted reported cache hit rate | 57.86% | 65.92% |
| Usage completeness | Complete | Complete |

| Pair and order | Off requests / input | On requests / input | Input reduction |
| --- | ---: | ---: | ---: |
| 1, off then on | 3 / 42,402 | 3 / 27,183 | 35.89% |
| 2, on then off | 4 / 52,716 | 3 / 27,183 | 48.44% |

Aggregate input decreased by 42.84%, including the effect of one additional off-policy request in pair 2. Pair 1 used equal request counts and still observed 35.89% lower total input in the compacted run. This strengthens the repeated observation for this fixture without isolating every stochastic/provider effect. The identical binary and unchanged fixture contract exclude a code-version change between these paired runs.

Cache-hit and non-hit input counts both decreased, but completion increased by 352 tokens. With hypothetical constant per-million rates Ru (uncached input), Rc (cached input) and Ro (output), compacted-minus-uncompacted token charge is `(-21552 * Ru - 19200 * Rc + 352 * Ro) / 1000000`. No provider prices, invoice totals, cache-write charges or hidden upstream retry charges were observed. This is a relation on reported tokens, not a measured monetary saving.

Two successful repetitions per policy are insufficient to establish quality non-inferiority or general coding performance. Provider cache state was not reset or controlled. Alternating order reduces a simple policy-order bias; it does not make these cold/warm-cache experiments. Raw sessions remain local at `/private/tmp/sigil-context-json-repeat-f56eh1qw/run/` and are not committed. The reports contain no authentication secret or raw model/tool content.

The first execution attempt was not run because automatic approval review returned a service 403. After the user requested retry, approval completed and the four observations above ran successfully. This infrastructure failure is not a fifth model trial and did not produce usage data.
