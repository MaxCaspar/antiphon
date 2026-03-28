# antiphon

Antiphon is a Rust TUI for running two AI agents in a turn-based terminal dialogue. It supports `claude`, `codex`, or a mixed pair, streams tokens live, shows reasoning/tool activity in side panels, and writes audit logs for every run.

> [!CAUTION]
> CLI agents launched by Antiphon run with full local permissions. They can read files, write files, and execute commands in the working directory without extra confirmation prompts. Use Antiphon only in repositories and with prompts you trust.

## What You Need

- Rust 1.85 or newer: [rustup.rs](https://rustup.rs)
- At least one agent CLI already installed and authenticated:
  - [Claude Code CLI](https://claude.ai/code) for `claude`
  - [OpenAI Codex CLI](https://github.com/openai/codex) for `codex` or `codex-api`

Antiphon installs the TUI itself. It does not install Claude Code or Codex for you.

## Install

If you just want to try Antiphon, install directly from GitHub:

```bash
cargo install --git https://github.com/maxcaspar/antiphon --locked
```

That places the `antiphon` binary in Cargo's bin directory, usually `~/.cargo/bin`. Make sure that directory is on your `PATH`.

To build from source instead:

```bash
git clone https://github.com/maxcaspar/antiphon
cd antiphon
cargo build --release
./target/release/antiphon -- "Start."
```

## Fastest First Run

The simplest setup is Claude on both sides:

```bash
antiphon -- "Design a rate-limiting strategy for this repo."
```

Mixed Claude/Codex session:

```bash
antiphon --agent-b codex -- "Review this API design."
```

Explicit turn count:

```bash
antiphon --agent-a claude --agent-b codex --turns 4 -- "Debate the CAP theorem."
```

Non-interactive mode:

```bash
antiphon --debug --turns 2 -- "Start."
```

Press `r` in the TUI to launch or relaunch the conversation. If `stdin` or `stdout` is not attached to an interactive terminal, Antiphon automatically falls back to debug mode instead of failing.

## Agent Modes

| Command | Use when | Notes |
|---|---|---|
| `claude` | You use Claude Code locally | Default for both agents |
| `codex` | You use the Codex CLI with its normal login/config | Streams Codex CLI events directly |
| `codex-api` | You want Codex forced into API-key auth with isolated config | Reads OpenAI settings from `.env` and uses a separate `CODEX_HOME` |

Open the agent chooser with `a` for Aria or `d` for Basil. Use `↑/↓`, `Enter`, and `Esc`.

## Configuration

Antiphon stores runtime data in your platform config directory:

| Platform | Default path |
|---|---|
| Linux | `~/.config/antiphon/` |
| macOS | `~/Library/Application Support/antiphon/` |
| Windows | `%APPDATA%\\antiphon\\` |

- Override the location with `ANTIPHON_HOME=/your/path`
- If the platform config directory cannot be created or written, Antiphon falls back to `./.antiphon` in the current working directory

For Claude-only usage, you usually do not need an Antiphon config file if `claude` already works in your shell.

For `codex-api`, create `~/.config/antiphon/.env` or set `ANTIPHON_HOME` and place `.env` there:

```bash
mkdir -p ~/.config/antiphon
cat > ~/.config/antiphon/.env <<'EOF'
OPENAI_API_KEY=your_key_here
OPENAI_MODEL=gpt-5.4
OPENAI_REASONING_EFFORT=medium
OPENAI_VERBOSITY=high
EOF
```

See [`.env.example`](./.env.example) for all supported variables, including `OPENAI_BASE_URL`, `CODEX_API_CMD`, and `CODEX_API_CODEX_HOME`.

## TUI Controls

### Setup and Editing

| Key | Action |
|---|---|
| `r` | Launch or relaunch |
| `w` | Edit the briefing prompt |
| `q` / `e` | Edit Aria / Basil system prompt |
| `a` / `d` | Open Aria / Basil agent chooser |
| `1`-`9` | Set turn count directly |
| `` ` `` | Enter an exact turn count |
| `s` | Open preset mode |
| `?` / `h` | Open or close the help modal |

### During a Run

| Key | Action |
|---|---|
| `p` | Pause or resume |
| `c` | Clear chat panes |
| `Esc` | Stop the run or back out of the deepest active mode |
| `Ctrl-Q` | Quit |

### View and Navigation

| Key | Action |
|---|---|
| `x` | Cycle routing mode |
| `y` | Toggle layout |
| `n` | Expand or collapse thinking |
| `b` | Toggle tmux side panes |
| `v` | Toggle mouse capture for text selection |
| `↑/↓`, `j/k`, `PgUp/PgDn` | Scroll |

## Routing Modes

| Mode | Behavior |
|---|---|
| `prompt->A` | The initial prompt goes only to Aria; after that, each agent sees the latest reply |
| `prompt->A+B` | Both agents receive the original prompt on their first turn |

## Layouts

Classic mode shows a single conversation pane plus a collapsible thinking rail.

Tri-pane mode puts Aria's thinking on the left, the conversation in the center, and Basil's thinking on the right. Tool activity and reasoning events remain in arrival order inside each agent's panel.

## Presets

Press `s` to manage presets. Presets store:

- briefing prompt
- both agent system prompts
- turn count
- routing mode
- layout and related UI settings
- selected agent commands

Inside preset mode:

| Key | Action |
|---|---|
| `j/k` or `↑/↓` | Move selection |
| `Enter` | Load selected preset |
| `Ctrl-S` | Save or update |
| `Ctrl-D` | Delete |
| `Esc` | Exit preset mode |

## Audit Logs

Each run writes logs under `<config-dir>/conversations/conv-<id>/`:

```text
conversation.jsonl
agent_a.jsonl
agent_b.jsonl
live.log
agent_a_live.log
agent_b_live.log
```

Use `--audit-log <PATH>` if you want the conversation output written somewhere else.

## CLI Reference

```text
antiphon [OPTIONS] [-- <INITIAL_PROMPT>]

Options:
  --agent-a <AGENT_A>      [default: claude]
  --agent-b <AGENT_B>      [default: claude]
  --turns <TURNS>          [default: 10]
  --debug
  --output <OUTPUT>        [default: text] [possible values: text, json]
  --audit-log <AUDIT_LOG>
  --quiet
  -h, --help               Print help
  -V, --version            Print version
```

## Release Checklist

Before telling other people to install Antiphon, confirm:

1. `cargo test`
2. `cargo build --release`
3. The README install command points at the correct repository
4. At least one supported agent CLI (`claude` or `codex`) is installed and works on a clean shell

## License

[MIT](LICENSE)
