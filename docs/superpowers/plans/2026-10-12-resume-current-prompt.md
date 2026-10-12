# Current-turn prompt reconstruction

Problem: pipe resume restores user/assistant/tool events, without a harness system prompt. run_messages rebuilt the missing prompt using the first historical user message, selecting stale skills and passing old task text to on_prompt_build.

Fix: use the most recent user message when adding a missing system prompt. Preserve the existing pre-built-system path and all historical messages. No provider, network, pricing or session-format changes.

- [x] Extend real CLI fake-LLM regression with first/second skills and a hook recording user_prompt.
- [x] Observe failure before the fix: resumed second task still had old-task skill and CURRENT_PROMPT:first.
- [x] Reverse the user-message search in the existing reconstruction path.
- [x] Verify new skill selection, CURRENT_PROMPT:second, full history and usage persistence, and ephemeral ordinary runs.
- [x] Independent read-only review found no concrete regression.
- [x] Validate 349 default and 347 containment tests; fmt, warnings-denied clippy, release build; 15 evaluator E2E tests, 8 usage-report tests, 8 cost tests and pipe-session CLI E2E passed.
- [x] Update bilingual documentation and create local commit after checks.

No paid model requests were made for this patch. Existing experiment reports retain their original binary fingerprints and observations.
