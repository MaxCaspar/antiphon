# codemap

Lean lookup table for agents. Keep entries short and grep-friendly.

## Core
| Path | Purpose | Tags |
|---|---|---|
| `Cargo.toml` | Rust package metadata, deps, CLI crate config, and release/install metadata for public distribution | config, build, release |
| `README.md` | Agent-first project handbook plus public install, prerequisites, first-run flow, and operator/runtime contracts | docs, onboarding, release |
| `src/main.rs` | Process entrypoint; exits with `run_cli()` code | entrypoint |
| `src/lib.rs` | Public modules + app boundary (`run`, `run_cli`), runtime home resolution with `ANTIPHON_HOME` override and `./.antiphon` fallback when the platform config dir is unavailable, startup `.env` loading from cwd then runtime home | boundary, wiring, env |
| `src/cli.rs` | Clap CLI args/options (`--agent-a`, `--agent-b`, `--turns`, `--debug`, `--output`, `--workspace`, etc.) | cli, parsing |
| `src/error.rs` | Typed app errors + stable error codes for terminal/runtime/process failures | errors, contracts |
| `src/app.rs` | Top-level orchestration for debug mode and crossterm TUI mode, automatic fallback to debug when no interactive terminal is available, explicit launch/relaunch loop via `r`, workspace-aware settings/audit resolution, launch-vs-active workspace tracking for deferred repo changes, and bounded 2s conversation shutdown with forced abort fallback on quit/relaunch/error | orchestrator, runtime, workspace |
| `src/conversation.rs` | Turn loop, transcript model, named routing modes (relay, prompt-to-both, PO-to-dev), pause/stop control, token + thinking + tool-activity event emission, bounded token micro-batching (`256B`/`8ms`/turn-end flush) before audit/UI dispatch, and workspace-root plumbing for agent cwd + audit metadata | conversation, state, routing, perf, workspace |
| `src/agent.rs` | Agent subprocess launch in full-permission mode + streaming parse (Claude/Codex/Codex-API/plain, incl. single-pass JSON decode dispatch per line for token/thinking/tool extraction from `output/items/response.output` plus `item.started`/`item.completed` `command_execution` and `file_change`), preserving raw `tool_type` for each tool event; includes `codex-api` alias -> `codex` mapping, forced API-key auth/model-provider config with default OpenAI model fallback `gpt-5.4` and default `model_verbosity="high"`, stable runtime-home-backed `CODEX_HOME`, workspace-root subprocess cwd, and `\n\n` separators between multiple non-delta Codex `agent_message` blocks in one turn | subprocess, parsing, permissions, workspace |
| `src/ui.rs` | Crossterm TUI: render, key handling, scrolling, launch/relaunch + routing selector + clear-chat action + explicit per-agent chooser state for command selection (`q` Aria / `e` Basil, choices `claude`/`codex`/`codex-api`, `↑/↓` move, `Enter` commit, `Esc` cancel) + hierarchical input precedence (workspace panel -> system-prompt -> brief -> turns -> agent chooser -> presets -> global controls -> scrolling) with `Esc` as deepest-context back/stop and `Ctrl-Q` quit + normal-mode rebinds for brief (`w`), workspace (`g`), direct turns (`1-9`), precise turns (`` ` ``), routing/layout (`x`/`y`), thinking/selection/tmux (`n`/`v`/`b`) + centered workspace overlay for repo switching/scope changes/suggestions + classic/tri-pane thinking layout (active thinking title emojis: Aria purple/abstract cycle, Basil nature cycle; chooser renders in the relevant classic rail or side column) + unified ordered thinking timeline state for reasoning text, tool rows, and message boundaries projected into per-message tri-pane segments so each message chunk keeps its original reasoning/tool order (`▶` use rows replaced in-place by `✓`/`✗` only when tool-call ids match; otherwise completion remains separate) with raw `[tool_type]` badges and command/query previews + wrap/expand + vanilla prompt briefing editor with cursor/selection editing (`Ctrl-A`, arrows, `Shift+arrows`, `Backspace/Delete`, `Ctrl-S`) + two-row operator footer with boxed state badge/chip strip + tmux panel toggle + per-agent system prompt boxes (TriPane inline boxes + Classic briefing overlay; preset mode uses a centered overlay in both layouts; workspace panel and `Esc` follow deepest-context exit semantics) | tui, interaction, routing, workspace, agent-selector, system-prompt |
| `src/workspace.rs` | Workspace/runtime path model, scope resolution (`Global` vs `RepoLocal`), registry persistence, settings bootstrap/import helpers, path normalization, and bounded recent/sibling suggestion ranking for repo switching | workspace, persistence, paths |
| `src/tmux.rs` | Optional tmux pane setup/teardown for live logs | tmux, tooling |
| `src/audit.rs` | Conversation/agent JSONL logs + live log files | logging, audit |
| `src/output/mod.rs` | Output module export | output |
| `src/output/render.rs` | Human/JSON error rendering | output, json |

## Tests
| Path | Purpose | Tags |
|---|---|---|
| `tests/cli.rs` | CLI smoke tests (`--version`, invalid flag, basic parse) | test, cli |
| `tests/integration_flow.rs` | End-to-end debug flow with mock agents, failure path, `--workspace`/cwd propagation checks, Codex multi-message separator regression, reasoning-event de-dup, and single-pass tool-event emission coverage | test, integration, workspace |
| `tests/snapshots.rs` | Snapshot tests for `--help` and JSON errors | test, snapshot |
| `tests/fixtures/mock_claude.sh` | Mock Claude-style JSON stream output | test-fixture, claude |
| `tests/fixtures/mock_codex.sh` | Mock Codex-style JSON stream output | test-fixture, codex |
| `tests/fixtures/mock_codex_interleaved_stream.sh` | Mock Codex JSON stream with reasoning/tool/reasoning/tool/reasoning interleaving plus final message for end-to-end ordering coverage | test-fixture, codex, reasoning, tools |
| `tests/fixtures/mock_codex_two_messages.sh` | Mock Codex output with two discrete `agent_message` completions in one turn | test-fixture, codex |
| `tests/fixtures/mock_codex_reasoning_stream.sh` | Mock Codex JSON stream with reasoning/thinking event variants for end-to-end event propagation tests | test-fixture, codex, reasoning |
| `tests/fixtures/mock_codex_tool_stream.sh` | Mock Codex JSON stream with command-execution start/completion events plus final message for tool-event emission regression coverage | test-fixture, codex, tools |
| `tests/fixtures/mock_fail.sh` | Fixture that exits non-zero for error-path coverage | test-fixture, failure |
| `tests/fixtures/mock_pwd.sh` | Fixture that prints subprocess cwd for workspace propagation coverage | test-fixture, workspace |

## Specs and Ops
| Path | Purpose | Tags |
|---|---|---|
| `specs/codemap.md` | This fast lookup table for agents | spec, index |
| `specs/plans/` | Scoped product and implementation plan documents used before coding changes | spec, planning |
| `.env.example` | Local environment template for API-based agent commands (`codex-api`) | config, env |
| `~/.config/antiphon/tui-settings.json` | Global cockpit settings under runtime home when a repo uses global scope | runtime-data, settings, workspace |
| `~/.config/antiphon/workspaces.json` | Global workspace registry: last workspace, recent repos, and per-repo scope preferences | runtime-data, workspace |
| `<repo>/.antiphon/tui-settings.json` | Repo-local cockpit settings when a workspace opts into repo scope | runtime-data, settings, workspace |
| `<runtime-home>/conversations/` | Global-scope audit outputs by conversation id | runtime-data, logs, workspace |
| `<repo>/.antiphon/conversations/` | Repo-local audit outputs by conversation id | runtime-data, logs, workspace |
| `target/` | Rust build artifacts | generated, build |
