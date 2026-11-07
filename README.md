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
  got, either split into labelled parts with their sizes or as the raw bytes; `/` finds text in it.
- **Edit the context by hand.** `/edit` opens the whole conversation in your editor as plain
  text: the system prompt, every message, tool call and result. What you save is what the
  model gets next.
- **Stop and continue.** <kbd>Esc</kbd> stops a reply at once and keeps what was written.
  <kbd>Enter</kbd> on an empty line picks it up again. Where the provider allows it, the reply
  continues from its last token instead of starting over. Errors that may pass, such as rate
  limits and overloaded servers, are retried up to three times, in plain view.
- **File tools**: read, list, find, search, write, edit, move and delete. Several reads run at the
  same time, and search walks the tree on every core. Edits and overwrites keep a file's line
  endings and byte order mark. Each edit or overwrite shows the lines it took out and put
  in, and a new file shows its first lines. A PNG, JPEG, GIF or WebP image that the model reads
  is shown to it, when the model takes images.
- **A shell** for builds, tests, git and scripts, held by Linux's Landlock to what the model
  may see: commands write only in its directories, the temporary directory and tool caches,
  cannot read your home outside them, and find hidden paths empty, all checked by the kernel
  for every program a command starts. Commands that do what a file tool does are refused, so
  hidden paths stay out of view and changes stay undoable. No container or other user; on
  systems without Landlock there is no shell unless you allow it ([how](#settings)). The few
  commands whose harm undo cannot reach, such as a force push, a publish, destroying remote
  infrastructure, dropping a table or a reboot, wait for your yes.
- **Web search and page reading** through Exa's MCP server, which needs no key; an `exa` entry
  in the settings replaces it (with your key in its address) or turns it off.
- **MCP servers**, on stdin and stdout or over HTTP, set the way other agents set them, so a
  server's own instructions can be copied in ([how](#settings)). Their tools join the model's
  own; what they change in the working directories undoes like a command's, and a tool its
  server calls destructive waits for your yes. A server runs as its own program, outside the
  limits on the shell and hidden paths.
- **Agents.** The model can start other agents on parts of a task, several at once: each has a
  fresh context and the same tools, and only its answer comes back, so the main conversation
  stays small. Their steps show as they work, their cost counts in yours, their changes undo
  like any other, and each one's whole conversation is kept beside yours.
- **Tools of your own.** Give the model any program to run, such as your build or tests,
  with its arguments put into the command line and no shell in between
  ([how](#tools-of-your-own)).
- **Undo.** Every change the agent makes is written to a journal. `/undo` takes changes back,
  the last one or the last few; `/rewind` goes back to an earlier message with every file change
  since. Commands, of the shell or your own tools, are undone the same way: the directories the
  model may use are looked at right before and after each one, and each file it changed is kept
  as it was (not inside directories `.gitignore` leaves out, such as build output; an ignored
  file such as `.env` is kept). A file you changed
  yourself in the meantime is left alone.
- **Instructions.** `AGENTS.md` in the working directory (or, without one, the file another
  agent reads: `CLAUDE.md`, `GEMINI.md`, `.cursorrules` and the like) and your own
  `~/.config/glasscat/AGENTS.md` go into the system prompt of a new conversation, where
  <kbd>Ctrl</kbd>+<kbd>T</kbd> shows them. A hidden file stays out.
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
- **Markdown** in replies is rendered: headings, lists and task lists, quotes, tables, links,
  bold, italic and struck text, and code in colour for the common languages and diffs, in your
  terminal's own palette.
  Your messages, the input, code blocks and changed lines sit on soft panels worked out from
  the terminal's background, light or dark; a terminal that does not report it gets none.
  A table too wide for the screen wraps its widest cells. Links and web addresses open with a
  click, even where an address wraps onto the next line.
- **Usage, cost and speed** of each request in the status bar: the context used out of the
  model's window, cache hits, time to the first token, tokens per second, and what all the
  requests so far cost together; `/usage` lists each request. When the provider's cache gives
  back much less than the last request sent, you are told what changed: the request, the
  provider it ran on, or nothing.
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
glasscat login                            # sign in to OpenRouter in the browser
glasscat                                  # full-screen session in this directory
glasscat "what does src/main.rs do?"      # one answer, printed to stdout
glasscat -c                               # pick up the last conversation held here
```

`glasscat login <provider>` signs in elsewhere: other providers take a pasted key, or one
piped in. Keys are kept in one file only you can read (`~/.local/share/glasscat/auth.json`;
`%LOCALAPPDATA%\glasscat` on Windows); `glasscat logout` removes it. A key variable such
as `OPENROUTER_API_KEY` overrides the stored key. Over SSH, the OpenRouter page shows a code
to paste instead.

With a prompt, the answer goes to stdout; tool calls, the lines each change took out and
put in, retries and the cost of the run go to stderr.

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
| <kbd>Tab</kbd> | complete a command or a file path; typing `/` lists the commands |
| <kbd>Esc</kbd> | stop the reply |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | the exact context sent to the model |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> | show or fold reasoning and long pasted messages |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> | undo in the input, say after <kbd>Ctrl</kbd>+<kbd>U</kbd> cleared a line by mistake; <kbd>Ctrl</kbd>+<kbd>Y</kbd> puts back what was deleted |
| <kbd>Shift</kbd>+drag | select text (in most terminals) |
| `/model <id>` | switch the model |
| `/resume` | earlier conversations held in this directory; `/resume N` picks one up |
| `/retry` | drop the last answer and its file changes, and ask again, say after switching the model |
| `/models <words>` | find models by id, with context window and price; on OpenRouter, `ZDR` marks those a provider keeping no data runs, and the word `zdr` keeps to them; `no tools` marks those that can only talk, which are sent no tools |
| `/rewind` | go back to an earlier message: the conversation as it was then, the file changes since taken back, the message in the input |
| `/undo` | undo the agent's last file change; `/undo 3`, the last three |
| `/changes` | the agent's file changes in this session, which `/undo` takes back in turn |
| `/access` | a tree of what the model may see, to hide or show paths; or `hide`, `show`, `allow` PATH |
| `/tools` | the model's tools: which only read, and what your own ones run |
| `/report` | open the model's last report about a glasscat fault as a new issue |
| `/edit` | change the context in `$EDITOR` before the next request |
| `/copy` | copy the last reply to the clipboard |
| `/settings` | edit the settings file; most of it applies at once |
| `/set <name> <value>` | set a request field for this session, such as `temperature 0.3`; `/set` lists them |
| `/provider` | who ran the last reply, and what they publish about your data |
| `/usage` | each request of this run: tokens in, from the cache and out, cost, time to the first token and in all, speed, and who ran it |
| `/login`, `/logout` | sign in to the provider, or remove its stored key |
| `/compact` | make the context shorter now: the older part becomes a summary |
| `/clear` | start a new conversation |
| `/help` | all commands and keys |

A long conversation keeps fitting the model's window on its own. From half the window, old
tool results the model can fetch again are cleared, when that frees a tenth of it. From 70%
the model writes a summary of the older part on the side, on the same cached prompt, so
nobody waits for it; at 85%, or when the provider says the context is too long, it takes the
older part's place. Your own messages stay word for word next to it, and the whole
conversation before is kept next to the saved one.

What a command only shows, such as `/help` or `/tools`, opens over the chat; <kbd>Esc</kbd>
closes it and leaves the chat where you had scrolled it.

## Providers

Any server that speaks the OpenAI chat completions API works. These are built in:

| `--provider` | Key variable (overrides `login`) | Continues a cut-off reply |
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

OpenRouter caches the prompt of Claude models only when the request asks for it, so for
`anthropic/` models glasscat adds `"cache_control": {"type": "ephemeral"}`. `/set` lists it
with the other request fields; `/set cache_control null` leaves it out.

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
| `shell` | the model's shell: `landlock` (the default: limited, and none without Landlock), `unrestricted` (reaches all you can, hidden paths too) or `off` |
| `mcp` | MCP servers by name: `{"command": "...", "args": [...], "env": {...}}`, or `{"url": "...", "headers": {...}}`; `"disabled": true` keeps one off |
| `confirm` | whether the few commands undo cannot take back wait for your yes (default `true`) |
| `instructions` | files in the working directory added to the system prompt (default: the first of `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `.github/copilot-instructions.md`, `.cursorrules`, `.windsurfrules`, `.clinerules`, `CONVENTIONS.md`) |
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

A tool of your own is outside the rules for hidden paths: it does exactly what the program
does. What it changes in the directories the model may use, `/undo` takes back.

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
| Every file change, old and new text | under the edit or write, lines taken out and put in; `/changes` lists them; in full in `~/.local/share/glasscat/edits/` |
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
