# goose-readline

A raw-terminal readline client for the [goose](https://github.com/block/goose)
agent, speaking ACP **in-process** (goose as a Rust library — no node, no
stdio, no child process). Rust rewrite of kilo-readline; crossterm replaces
only escape-sequence decoding, raw mode, and bracketed paste — every line of
editing, layout, history, completion, and render logic is hand-ported.

## Layout

- `src/editor/` — the raw-mode line editor (port of `rawinput.ts` + `width.ts`
  + `history.ts`): logical `lines[]` buffer with char-index cursor,
  single-frame redraws, cached-row diffing, tall mode with a hidden-row
  viewport, bottom-pinned mode, history drafts, Ctrl+R reverse search, and
  Tab-completion cycling with a 2-row overlay.
- `src/agent/` — in-process goose wiring (`GooseAgentConnection` →
  `Client.builder()`), env parity with `scripts/run-goose.sh`, and the
  `--spawn` fallback (a `goose-acp` binary over stdio).
- `src/app/` — the event loop (crossterm + agent channel + heartbeat),
  transcript renderer (markdown/tables/code fences/diffs/tool blocks/
  thoughts), permission flow (local-allow auto-approve, config deny, numbered
  menu with always-allow persistence), slash commands, usage endpoints
  (zai/codex/openrouter), and model-state persistence.
- `tests/acp_integration.rs` — headless integration test: initialize,
  new session, prompt, assert the reply arrives.

## Dev

```sh
just build          # cargo build
just test           # cargo test
just integration    # requires a Z.AI key (from kilo's auth.json) + network
just run            # run the client in this directory
```

CLI: `goose-readline [-c] [-s <session-id>]`. The env var
`GOOSE_READLINE_SPAWN="<binary> [args]"` switches to the stdio-spawn
transport.

## Notes

- The editor is fully unit-tested (`cargo test --lib`): type/submit rules,
  word ops, history drafts, search, paste, completion cycling/revert, tall
  mode, boundary cues, wrap math.
- Terminal restore is guaranteed on panic (hook) and on exit.
- Input during agent output is filtered out (paste state included); Ctrl+C
  cancels the running turn; double Ctrl+C at the empty prompt exits.
