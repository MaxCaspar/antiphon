What happens when two AI agents talk to each other? **Antiphon** is a terminal UI (TUI + CLI) built around that question — two agents, 🪻**Aria** and 🌿**Basil**, in a turn-based dialogue. 

Point them at a feature, a design question, or a fictional world and watch them go. They're coding agents at heart, so they'll want to build things, which makes them surprisingly effective on real problems.

*FYI: You can use this as a double [ralph-loop](https://ghuntley.com/loop/) -> one builds the other reviews. Then just start it again with fresh context.*

Ships with presets. MIT licensed — v1, fork it freely.

![Screenshot](assets/antiphon-TUI.png)

> [!CAUTION]
> Agents launched by Antiphon run with full local permissions. They can read, write, and execute in the working directory without extra prompts. Only use it in repos and with prompts you trust.

## Install

**Install the binary:**

```bash
cargo install --git https://github.com/maxcaspar/antiphon --locked
```

Then from anywhere:

```bash
antiphon
```

Or to build from source:

```bash
git clone https://github.com/maxcaspar/antiphon
cd antiphon
cargo build --release
./target/release/antiphon -- "Start."
```

You'll need:
- Rust 1.85+: [rustup.rs](https://rustup.rs)
- At least one agent CLI installed and authenticated — [Claude Code](https://claude.ai/code) or [OpenAI Codex](https://github.com/openai/codex)

## Usage

Both agents default to Claude:

```bash
antiphon -- "Design a rate-limiting strategy for this repo."
```

Mixed pair:

```bash
antiphon --agent-b codex -- "Review this API design."
```

Explicit turns:

```bash
antiphon --agent-a claude --agent-b codex --turns 4 -- "Debate the CAP theorem."
```

Press `r` in the TUI to launch or relaunch. Press `s` to load a preset.

## TUI Controls

| Key | Action |
|---|---|
| `r` | Launch or relaunch |
| `w` | Edit the briefing prompt |
| `q` / `e` | Edit Aria / Basil system prompt |
| `a` / `d` | Open agent chooser |
| `s` | Open preset mode |
| `p` | Pause or resume |
| `Esc` | Stop run or back out |
| `Ctrl-Q` | Quit |
| `x` | Cycle routing mode |
| `y` | Toggle layout |
| `b` | Toggle tmux side panes |
| `?` / `h` | Help |

## Agent Modes

| Command | Notes |
|---|---|
| `claude` | Claude Code CLI (default) |
| `codex` | Codex CLI with normal login |
| `codex-api` | Codex forced into API-key auth — reads from `.env` |

For `codex-api`, create `~/.config/antiphon/.env`:

```bash
OPENAI_API_KEY=your_key_here
OPENAI_MODEL=gpt-4o
```

See [`.env.example`](./.env.example) for all options.

## Presets

Press `s` to save or load presets. A preset stores the briefing, both system prompts, turn count, routing mode, layout, and agent selection.

## Audit Logs

Each run writes logs to `<config-dir>/conversations/conv-<id>/` — full JSONL transcripts for both agents.

## CLI Reference

```text
antiphon [OPTIONS] [-- <INITIAL_PROMPT>]

  --agent-a <AGENT>    [default: claude]
  --agent-b <AGENT>    [default: claude]
  --turns <N>          [default: 10]
  --debug
  --audit-log <PATH>
  -h, --help
  -V, --version
```

## Uninstall

```bash
cargo uninstall antiphon
rm -rf ~/.config/antiphon        # Linux
rm -rf ~/Library/Application\ Support/antiphon  # macOS
```

## License

[MIT](LICENSE)
