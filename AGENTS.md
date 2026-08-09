# AGENTS.md — Work Contract for AI Agents

sigil is a Rust + LuaJIT coding agent with a self-evolving harness. Phase 0:
minimal agent loop (OpenAI-compatible provider + ReAct + bash tool). This
file tells agent tooling how to work here without breaking things.

## Build & Test

```bash
cargo build                # debug
cargo build --release      # release (strip + lto, single binary)
cargo test                 # unit tests (agent loop, SSE parser)
cargo fmt -- --check       # must be zero diff
cargo clippy --all-targets -- -D warnings   # must be zero warnings
```

- Rust 2024 edition, `rust-version = "1.97"`.
- Crate name is `sigil-agent` (crates.io name), binary is `sigil`.
- CI (.github/workflows/ci.yml): fmt → clippy → test → release build, with
  `--locked` so Cargo.lock drift is a hard failure.

## Architecture

```
src/
├── main.rs          # entry: CLI parse → provider → pipe mode
├── cli.rs           # clap args (--model, --base-url, --api-key, --max-steps, -p)
├── config.rs        # env-var config: SIGIL_MODEL / SIGIL_BASE_URL / SIGIL_API_KEY
├── agent/mod.rs     # ReAct loop: stream → parse tool call → execute → repeat
│   └── tests.rs     # unit tests (tool-call extraction, SSE parsing)
├── llm/
│   ├── mod.rs       # re-exports the provider surface
│   ├── provider.rs  # LlmProvider trait (name, health, chat_stream) + types
│   └── openai.rs    # OpenAI-compatible client (vLLM/Ollama/llama.cpp/DeepSeek)
└── tools/
    ├── mod.rs       # Tool trait + ToolRegistry (unified lookup)
    └── bash.rs      # bash tool: run a command, capture stdout/stderr
```

Key contracts:
- `LlmProvider` trait is the only seam between the agent loop and backends.
  New providers implement `chat_stream(ChatRequest) -> ChatStream`.
- `ToolRegistry` is keyed by tool name, registration order preserved for
  stable spec ordering in prompts.
- Phase-0 tool-call convention: the model emits a fenced ```` ```json ```` block
  `{"tool": "...", "arguments": {...}}`; `agent::extract_tool_call` parses it.
  Native function-calling delta accumulation is Phase 1 (`parse_tool_calls` /
  `tool_call_to_message` in openai.rs are pre-written, `#[allow(dead_code)]`).

## Key Patterns

- **Provider-agnostic loop**: the agent loop never names a vendor; everything
  goes through `LlmProvider`. This is what makes fusion panels / local
  llama.cpp / vLLM all drop in later.
- **Tools never fatal**: `ToolRegistry::execute` converts tool errors into
  tool output strings so the model sees them — the loop never dies on a tool.
- **SSE robustness**: `parse_stream` handles `data:` frames, `[DONE]`,
  `reasoning_content` (thinking), and bad chunks (error, not panic).
- **Phase-1 reservations**: `parse_tool_calls`, `tool_call_to_message`,
  `error_output`, `LlmProvider::name/health` are written but `allow(dead_code)`
  until their phase lands. Do not delete them; wire them up instead.

## Risk Gates

- **AUTO-APPROVED**: changes under `src/`, adding cargo dependencies, tests.
- **REQUIRES APPROVAL**: changing `Cargo.toml` package identity (name/edition/
  rust-version), the default `SIGIL_BASE_URL`, CI workflows, LICENSE.
- **BOUNDARY**: `.github/workflows/release.yml` (publishing), anything that
  changes the binary's network default (default base URL points at the user's
  vLLM at 10.10.10.115:8010).

## Conventions

- Default branch: `main`. README is English-first; Chinese version lives in
  README_cn.md with cross-links at the bottom. Keep both in sync when
  changing README content.
- Architecture diagrams in README use Mermaid (GitLab 9.1.3 compatible:
  no `<br/>`, no `#` in node text, no emoji).
- License: MIT (Copyright alitrack).
- Commit style: conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`).
- Version bumps: README top + README changelog + Cargo.toml version, one
  commit, then tag `vX.Y.Z` for the release workflow.
