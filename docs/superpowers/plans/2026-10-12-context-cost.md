# Explicit-price context cost analysis

Goal: analyze existing evaluator reports without model calls or automatic price lookup.

Design: accept one paired report and optional caller-provided per-million uncached-input, cached-input and output rates plus currency. Aggregate from individual results, not the cached summary. Reject duplicate/missing pair members and mixed model pricing. Include failed attempts in total spend; cost per successful task is undefined at zero successes. Require complete usage and cache-token coverage for an estimate, except identical input rates do not need a cache split. Incomplete observations stay unknown. Without prices, output the observed token delta and symbolic charge relation only. Never treat estimates as invoices or recommend a policy based on them.

- [x] Add failing tests for cache-dependent cost reversal, missing metadata, failed attempts, pairing validation and invalid rates.
- [x] Implement decimal arithmetic and safe CLI reporting with all-or-none explicit rates.
- [x] Verify against committed real reports using symbolic analysis and clearly hypothetical rates.
- [x] Independent review and bilingual docs; preserve unknown actual service prices.
- [x] Local commit after regression and whitespace checks.


Validation: 8 offline cost tests passed, including cache-loss cost reversal under hypothetical prices, missing telemetry, zero-success spend, invalid rates, duplicate/unmatched or mismatched fixtures and CLI privacy. The decimal-exponent regression fails without its guard and passes after restoration. Independent review found no important accounting/CLI issue. Applied the unpriced analyzer to the real repeat report: symbolic delta is -21,552 uncached input, -19,200 cached input and +352 completion tokens. The monetary estimate and currency remain null, because the user does not know the actual service rates. The symbolic output is committed separately; hypothetical TEST calculations are verification only and are not claimed as provider pricing. No model/network calls were made.
