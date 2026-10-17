# gray-repl

Persistent per-session CPython namespace as a gray sidecar plugin. Port of the
*mechanics* of prime-agent's `rlm` kernel (prime-agent is MIT-licensed; no
prime-agent code is copied here — this is a clean reimplementation of the
protocol ideas in `prime-agent-runtime/src/rlm/repl.md`).

## What it does

prime-agent gives its model exactly one tool — `ipython` — and binds every
other capability into a persistent Python namespace, so multi-step sequences
happen inside one cell and intermediate results live as variables *outside*
the message context. gray-repl ports the kernel half of that design:

- `repl_exec {code, timeout_ms?}` — run a cell in this session's namespace.
  One globals dict per session; imports, variables, functions survive across
  calls. A trailing expression returns its `repr` (also bound to `_`).
- `repl_vars` — list live user-defined names with types.
- `repl_reset` — kill the kernel; next exec starts a fresh namespace.
- `repl_state {action: save|load, path?}` — pickle snapshot of the namespace
  (default `~/.gray/repl/<session>.pkl`), saved/restored name-by-name.
- `/repl status|vars|reset|save [path]|load [path]` — same control as a command.

## Deliberate differences from prime's kernel

- Sync `exec` only — no top-level `await`, no persistent event loop, no
  background asyncio tasks (prime compiles with `PyCF_ALLOW_TOP_LEVEL_AWAIT`).
- No display/MIME side channel, no MCP bridge, no bash handles.
- Snapshots use `pickle` (stdlib), not `dill`; per-name skip on pickle failure,
  plus a saved/skipped report. Unattributed writes outside a cell go to the
  plugin's stderr log (prime ships them as `id:null` stream events).
- Cell timeout kills the kernel (prime delivers `KeyboardInterrupt` via
  SIGINT). Default 30s, max 600s; on expiry the namespace is lost and the
  next call spawns a fresh kernel — reported plainly in the tool result.

## Wire

Single-file Rust sidecar per `PLUGIN-CONTRACT.md` (edition 2024, serde_json
only): `manifest` argv probe, `sidecar`/bare-stdin NDJSON loop, one reply per
request, `-32601` for unknown methods, `plugin/shutdown` exits promptly.

Verified: `cargo build --release` clean; `drive.py` all-PASS; live session
test — persistence, `x + len(big) == 5040` after reset+load round-trip.
