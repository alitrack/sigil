# harness-locations

> Where sigil's global harness assets live on THIS machine — probed on
> disk 2026-08-19, re-probed 2026-08-26. No `SIGIL_*` env overrides
> exist in the code or on this box; only `--harness-dir` can redirect
> `default_harness_dir()`.

This box runs sigil's Windows build (repo `D:\wsl2\sigil`,
`x86_64-pc-windows-msvc`). Resolved global state:

- **Global harness dir** (`system_prompt.md` + `skills/`):
  `C:\Users\DecisionLinnc\AppData\Roaming\sigil\harness`
  (= `%APPDATA%\sigil\harness`; code:
  `default_harness_dir() = dirs::config_dir()/sigil/harness`,
  src/harness/mod.rs:71)
- Scripted Lua tools: `C:\Users\DecisionLinnc\AppData\Roaming\sigil\tools`
- Memory file: `C:\Users\DecisionLinnc\AppData\Roaming\sigil\memory.md`
- Config (absent on this box = built-in defaults): `C:\Users\DecisionLinnc\AppData\Roaming\sigil\config.toml`
- Sessions / hooks / agents / delegates: `C:\Users\DecisionLinnc\.sigil\`
  (= `~/.sigil/`; code: `dirs::home_dir()/.sigil`)

2026-08-26 probe: harness dir holds `system_prompt.md` (380 B — the
built-in default prompt, uncustomized) and `skills/`; `tools/` and
`memory.md` (1.5 KB) present; config.toml absent. On Linux the same
code resolves harness/tools/memory/config to `~/.config/sigil/` (NOT
`~/.sigil/harness`); WSL is installed on this box but no distro is
accessible (E_ACCESSDENIED), so WSL is not the active state root.

## Keeping the installed copy in sync

This repo file (`harness-skill/harness-locations.md`) is the source of
truth; the live copy sigil sessions actually load is
`%APPDATA%\sigil\harness\skills\harness-locations.md`. Refresh the
install with (needs full shell access — agent sandboxes can't write
outside the workspace):

```powershell
Copy-Item D:\wsl2\sigil\harness-skill\harness-locations.md $env:APPDATA\sigil\harness\skills\
```