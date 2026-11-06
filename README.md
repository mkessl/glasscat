<div align="center">

# glasscat

**An AI agent you can see through.**

A fast terminal agent in Rust: a language model with tools, working on your computer.<br>
Every byte sent to the model is yours to read, and every change it makes is yours to undo.

[Build](#build) · [Use](#use) · [Providers](#providers) · [Settings](#settings) · [What you can see](#what-you-can-see) · [Support](#support-the-project)

<br>

<img src="assets/demo.svg" alt="glasscat fixes two bugs in a small Python project, showing each edit, what it cost and how much the cache gave back" width="840">

</div>

---

> [!NOTE]
> glasscat is at an early stage. What is described here works today; much more is coming,
> at a measured pace.

## Why another agent

Most agents hide what they send to the model: the system prompt, the tool definitions, what
was cut from the conversation. glasscat shows all of it, exactly as sent. You own the agent,
and the model is told so: it answers questions about itself plainly.

It is also built to be quick. It is one small native binary that starts at once, streams
every token as it arrives, and stops the moment you press <kbd>Esc</kbd>.

## What it does today

- **Full-screen terminal interface** with its own renderer: only changed cells are redrawn,
  and frames are synchronized, so nothing flickers.
- **The exact context, live.** <kbd>Ctrl</kbd>+<kbd>T</kbd> shows the last request the model
  got, either split into labelled parts with their sizes or as the raw bytes.
- **Edit the context by hand.** `/edit` opens the whole conversation in your editor as plain
  text: the system prompt, every message, tool call and result. What you save is what the
  model gets next.
- **Stop and continue.** <kbd>Esc</kbd> stops a reply at once and keeps what was written.
  <kbd>Enter</kbd> on an empty line picks it up again. Where the provider allows it, the reply
  continues from its last token instead of starting over. Errors that may pass, such as rate
  limits and overloaded servers, are retried up to three times, in plain view.
- **File tools**: read, list, find, search, write, edit and delete. Several reads run at the
  same time, and search walks the tree on every core. Edits and overwrites keep a file's line
  endings and byte order mark, and each edit shows the lines it took out and put in.
- **Tools of your own.** Give the model any program to run, such as your build or tests,
  with its arguments put into the command line and no shell in between
  ([how](#tools-of-your-own)).
- **Undo.** Every change the agent makes is written to a journal. `/undo` takes changes back
  one at a time, and leaves alone a file you changed yourself in the meantime.
- **Instructions.** `AGENTS.md` in the working directory and your own
  `~/.config/glasscat/AGENTS.md` go into the system prompt of a new conversation, where
  <kbd>Ctrl</kbd>+<kbd>T</kbd> shows them. A hidden `AGENTS.md` stays out.
- **Hidden paths.** Paths given with `--hide` do not exist as far as the model can tell. It
  gets the same answer as for a missing file. Nothing outside the working directory and the
  folders you allow can be reached, through symlinks or `..` either. You are told each time
  something was kept from the model, and why. `/access` opens your folders as a tree: pick a
  file or folder, with the keys or the mouse, and <kbd>Space</kbd> hides it from the model or
  shows it again. Files the model has read or changed are marked. <kbd>s</kbd> keeps the
  rules in the settings for next time. Or type them: `/access hide PATH`, `/access show PATH`,
  `/access allow PATH`.
- **Reasoning** from thinking models streams in, then folds away; <kbd>Ctrl</kbd>+<kbd>O</kbd>
  opens it.
- **Markdown** in replies is rendered: headings, lists, quotes, tables, links, bold and italic,
  and code in colour for the common languages and diffs, in your terminal's own palette.
  A table too wide for the screen wraps its widest cells.
- **Usage, cost and speed** of each request in the status bar: the context used out of the
  model's window, cache hits, time to the first token, tokens per second, and what all the
  requests so far cost together. When the provider's cache gives back much less than the last
  request sent, you are told what changed: the request, the provider it ran on, or nothing.
- **Reports, not telemetry.** When the model meets a fault of glasscat itself, it can write it
  up with only what the fault needs. The report is saved on your machine and nothing is sent;
  `/report` opens it as a new GitHub issue that you read and send yourself.

## Build

You need [Rust](https://rustup.rs) 1.95 or newer.

```sh
git clone https://github.com/mkessl/glasscat
cd glasscat
cargo install --path .
```

This puts `glasscat` in `~/.cargo/bin`. To build without installing, run
`cargo build --release`; the binary is `target/release/glasscat`.

## Use

```sh
export OPENROUTER_API_KEY=sk-or-...
glasscat                                  # full-screen session in this directory
glasscat "what does src/main.rs do?"      # one answer, printed to stdout
glasscat -c                               # pick up the last conversation held here
```

With a prompt, the answer goes to stdout; tool calls, retries and the cost of the run go to
stderr.

| Option | Meaning |
| --- | --- |
| `-c`, `--continue` | pick up the last conversation held in this directory |
| `--provider <name>` | where the model runs (default `openrouter`) |
| `-m`, `--model <id>` | the model to use; each provider has a default |
| `--allow <dir>` | another folder the agent may work in (repeatable) |
| `--hide <path>` | a path the model must not see (repeatable) |
| `--proxy <url>` | send requests through a proxy: `socks5://host:port`, `http://user:pass@host:port` |
| `--show-request` | print each exact request to stderr |
| `-V`, `--version` | print the version |

Inside a session:

| Key or command | Does |
| --- | --- |
| <kbd>Enter</kbd> | send; on an empty line, continue a reply that did not finish |
| <kbd>Shift</kbd>+<kbd>Enter</kbd>, <kbd>Alt</kbd>+<kbd>Enter</kbd> or <kbd>Ctrl</kbd>+<kbd>J</kbd> | new line |
| <kbd>↑</kbd> <kbd>↓</kbd> | earlier messages, also from past conversations held here |
| <kbd>Tab</kbd> | complete a command; typing `/` lists them |
| <kbd>Esc</kbd> | stop the reply |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | the exact context sent to the model |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> | show or fold reasoning |
| <kbd>Shift</kbd>+drag | select text (in most terminals) |
| `/model <id>` | switch the model |
| `/resume` | earlier conversations held in this directory; `/resume N` picks one up |
| `/retry` | drop the last answer and ask again, say after switching the model |
| `/models <words>` | find models by id, with context window and price |
| `/undo` | undo the agent's last file change |
| `/access` | a tree of what the model may see, to hide or show paths; or `hide`, `show`, `allow` PATH |
| `/tools` | the model's tools: which only read, and what your own ones run |
| `/report` | open the model's last report about a glasscat fault as a new issue |
| `/edit` | change the context in `$EDITOR` before the next request |
| `/copy` | copy the last reply to the clipboard |
| `/settings` | edit the settings file; most of it applies at once |
| `/set <name> <value>` | set a request field for this session, such as `temperature 0.3`; `/set` lists them |
| `/provider` | who ran the last reply, and what they publish about your data |
| `/clear` | start a new conversation |
| `/help` | all commands and keys |

What a command only shows, such as `/help` or `/tools`, opens over the chat; <kbd>Esc</kbd>
closes it and leaves the chat where you had scrolled it.

## Providers

Any server that speaks the OpenAI chat completions API works. These are built in:

| `--provider` | Key variable | Continues a cut-off reply |
| --- | --- | :---: |
| `openrouter` | `OPENROUTER_API_KEY` | ✓ (depends on the model) |
| `groq` | `GROQ_API_KEY` | ✓ |
| `deepseek` | `DEEPSEEK_API_KEY` | ✓ |
| `mistral` | `MISTRAL_API_KEY` | ✓ |
| `kimi` | `MOONSHOT_API_KEY` | ✓ |
| `ollama` | none, local | asks again |
| `llamacpp` | none, local | ✓ |
| `lmstudio` | none, local | asks again |

OpenRouter's default model, `openrouter/free`, costs nothing, so you can try glasscat with
a free OpenRouter key. Free models may keep what you send, or train on it; `/provider` shows
what the provider of each reply publishes about that. To have OpenRouter use only providers
that keep nothing, put `"request": {"provider": {"zdr": true}}` in the settings, with a
model that such providers serve.

## Settings

Settings live in `~/.config/glasscat/settings.json` (on Windows, `%APPDATA%\glasscat`).
Every field is optional, and options on the command line win. `/settings` opens the file in
your editor and applies it when you close it, without a restart: request fields, proxies,
allowed and hidden paths, and your own tools. Add any OpenAI-compatible server under
`providers`:

```json
{
  "provider": "vllm",
  "providers": {
    "vllm": {
      "base_url": "http://localhost:8000/v1",
      "model": "Qwen/Qwen3-8B",
      "prefill": "none"
    }
  },
  "hide": [".env", "secrets"]
}
```

| Field | Meaning |
| --- | --- |
| `provider`, `model` | used when none is given |
| `providers` | more providers by name, or changes to built-in ones: `base_url`, `model`, `key_env` (the variable holding the key), `prefill`, `proxy`, `request` |
| `allow`, `hide` | like `--allow` and `--hide`, added to them |
| `request` | fields set in every request body, such as `temperature` or `reasoning`; `null` removes one |
| `models` | settings for one model by its id; for now its own `request` fields, over the others |
| `tools` | tools of your own: programs the model may run, see below |
| `instructions` | files in the working directory added to the system prompt (default `["AGENTS.md"]`) |
| `proxy` | a proxy for every provider; a provider's own `proxy` wins, and `--proxy` wins over both |

Two accounts with one provider are two entries with the same `base_url`, each with its own
`key_env` and, if you like, its own `proxy`; `--provider` picks one.

`prefill` says how the server continues a cut-off reply: `plain` (a trailing assistant
message), `prefix` or `partial` (that flag on the message), or `none` to ask again.

With no proxy set, the usual `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` variables apply.

### Tools of your own

Give the model a program to run, and only that program:

```json
{
  "tools": {
    "check": {
      "description": "Compile the project and report errors.",
      "command": ["cargo", "check", "--message-format=short"],
      "read_only": true
    },
    "grep": {
      "description": "Search files for a regular expression.",
      "command": ["rg", "-n", "{pattern}", "{paths}"],
      "parameters": {
        "type": "object",
        "properties": {
          "pattern": { "type": "string" },
          "paths": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["pattern"]
      }
    }
  }
}
```

`{name}` in the command becomes the model's argument of that name; a list becomes several
arguments, and a missing one is left out. No shell is involved, so nothing in an argument is
run as a command. The program runs in the working directory and is stopped after `timeout`
seconds (default 120) or when you press <kbd>Esc</kbd>; the model gets the exit status and
the output. `read_only` lets it run at the same time as other reading calls.

A tool of your own is outside the rules for hidden paths, and `/undo` cannot take back what
it changes: it does exactly what the program does.

## What you can see

| | |
| --- | --- |
| The system prompt and tool definitions | <kbd>Ctrl</kbd>+<kbd>T</kbd>, word for word, before the first request too |
| Which tools the model has, and what each one runs | `/tools` |
| The exact bytes of each request | <kbd>Ctrl</kbd>+<kbd>T</kbd>, then <kbd>r</kbd>; or `--show-request` |
| Size of each part of the context | <kbd>Ctrl</kbd>+<kbd>T</kbd> |
| The whole context, to change as you like | `/edit` |
| What a tool kept from the model, and why | under the tool call |
| Tokens in and out, cache hits, cost, and the sum of it so far | status bar |
| Why the cache missed | under the reply, when it gave back less than half |
| How much of the model's context window is used | status bar, when the provider's model list says |
| Time to the first token, time in all, output speed | status bar |
| Which model and provider ran each reply | status bar, when a router such as OpenRouter says |
| That provider's quantization, data retention, location and privacy policy | `/provider` (OpenRouter) |
| The model's reasoning | as it streams; <kbd>Ctrl</kbd>+<kbd>O</kbd> afterwards |
| Every file change, old and new text | under the edit, lines taken out and put in; in full in `~/.local/share/glasscat/edits/` |
| Every conversation | `~/.local/share/glasscat/conversations/` |
| Reports the model wrote about faults of glasscat | `~/.local/share/glasscat/reports/` |

## Support the project

glasscat is free and open source, and lives on donations. If it is useful to you, thank you
for any help.

| Network | Address |
| --- | --- |
| Bitcoin | `bc1q33zlf926g3uvul7e4g6d3gh4la95rhtxlvc7k5` |
| Bitcoin Lightning | `extrovertedgarlic4467@cake.cash` |
| Ethereum and EVM chains | `0x1247B716f1e83ce207A3dDd1d1888ecb709b439A` |
| Monero | `45SGSEBddGWNYYsHomK5q9TMNJgGu3WQsfhUPpdP4uxuLGMeKGHi7X34YRVNC8uFkc7CQmGsCFQZvZfAaEjSHRvM8UDPHju` |
| Solana | `D6zrmUKUryVWw3VxGAvmukjoxLLhkrCVxaA3wZzbhWDB` |
| Tron | `TJfmaL4h569b8Q2K9S5eVbLBzKCAQacr6t` |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
