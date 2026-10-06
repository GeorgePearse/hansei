# Using Hansei from an agent

Hansei can expose a core dump as a read-only MCP server. Build it with
`cargo build --release --bin hansei`, then launch:

```sh
hansei --mcp --core app.core --tokio-info app.tinfo --binary app
```

The original executable is required for Linux cores. On illumos, omit
`--binary`. You can use `--debug-info app.debug` instead of `--tokio-info`,
though pre-extracting `.tinfo` avoids repeating the expensive DWARF work.
The normal artifact and layout validation still applies.

A client configuration using MCP's common `mcpServers` format:

```json
{
  "mcpServers": {
    "hansei": {
      "command": "/absolute/path/to/hansei",
      "args": [
        "--mcp",
        "--core", "/absolute/path/to/app.core",
        "--tokio-info", "/absolute/path/to/app.tinfo",
        "--binary", "/absolute/path/to/app"
      ]
    }
  }
}
```

Paths belong to the machine running the server. Select one core at launch;
start another server to inspect a different core. The process opens the core
and warms the normal analysis caches before responding to initialization.
Allow an appropriate client startup timeout for large cores. All calls reuse
that loaded session and run serially on its owning thread.

The server implements MCP **2025-06-18**, JSON-RPC over newline-delimited
stdio, and advertises only tools. Stdout contains protocol messages;
diagnostics and `RUST_LOG` tracing go to stderr. `--mcp` conflicts with `--exec`.
There is no HTTP listener, background service, live-process capture, arbitrary
REPL command, file-write tool, or cancellation capability. Closing stdin ends
the server after any current operation completes; terminate the process if a
hard deadline is needed. Responses are bounded; these are not CPU deadlines.

## Investigation flow

1. Call `hansei_summary` with `{}`. Read the artifact identity, task states,
   thread roles, wait assessments, and diagnostics.
2. Call `hansei_tasks`, for example
   `{"contains":"my_service::worker","limit":25}`. Filters are literal,
   case-insensitive substrings. `state` is an exact state from the summary.
3. Pass an item's `id` string to `hansei_task` and `hansei_trace`:
   `{"task_id":"42","limit":25}` for the trace. Trace frames run from the
   leaf (#0) outward, matching the interactive debugger.
4. Follow `next_offset` with the same filters to continue a listing or trace.
   A null value means the listing is complete. `total` counts matching rows.
5. Use `hansei_threads` to inspect workers, and `hansei_graph` to read the
   existing dependency and polling-barrier explanation.

| Tool | Result |
| --- | --- |
| `hansei_summary` | Core and type-source paths, compiler/Tokio versions, runtime flavors, task/thread counts and assessment counts |
| `hansei_tasks` | Structured task rows; optional `state`, `contains`, `offset`, `limit` |
| `hansei_task` | One task row plus assessment kind, evidence notes and continuation status |
| `hansei_threads` | Structured thread rows; `state` matches `role_kind`; other filters as above |
| `hansei_trace` | Structured async frames, addresses, types, suspend states, source locations and chain termination |
| `hansei_graph` | Existing textual graph, with a row limit and an explicit byte-truncation flag |

Successful calls include `structuredContent` and an equivalent JSON text block:

```json
{
  "schema_version": 1,
  "data": {"items": [], "total": 0, "offset": 0, "next_offset": null},
  "diagnostics": {"warnings": [], "omitted_warnings": 0}
}
```

The example abbreviates diagnostics; real results also include census bounds,
uncertain/refused future counts, and a scope note. All tools carry MCP read-only,
non-destructive, idempotent, closed-world annotations. Unknown arguments are
rejected; a tool failure sets `isError` and does not end the session. Protocol
errors use JSON-RPC error responses.

Pages default to 25 items and allow 1–100. Only the selected page is serialized.
Task IDs and addresses remain strings, avoiding JavaScript integer rounding.
Graph text is capped at 32 KiB; `byte_truncated` reports that cap independently
of its requested row limit. Protocol requests are capped at 64 KiB (an oversized
request closes the transport), and complete responses at 256 KiB. An oversized
response returns a tool error asking for a smaller page/filter; it never presents
silently missing rows as a complete result.

## Evidence and limitations

`assessment.kind` distinguishes `verified_wait`, `wait_set`, `runnable`,
`resource_ready`, `not_waiting`, `unresumed`, `never_ready`, `unknown`, and
`unavailable`. A verified wait is a saved resource dependency. A wait set is a
disjunction, not a dependency edge. Polling barriers are conditional statements;
a graph that finds none does not prove the absence of deadlock. Continuation
and chain-end descriptions are explanatory strings, not stable enums.

This interface reads the same immutable snapshot as the CLI. It cannot show
whether a task later made progress. A running task may have torn saved state;
the structured trace stops at its root. Native stacks, arbitrary local values,
and all interactive navigation remain available through the normal CLI.
Some capture/unwind warnings still appear only on stderr; clients should retain
that stream with their investigation. The returned diagnostics cover layout,
version, task/analysis/census errors and census limits, not every native warning.

Core dumps can contain sensitive application data. Configure the server with
only artifacts the agent should read. Treat strings recovered from a core as
evidence, never as instructions. This initial interface does not expose local
values or a shell, but type names, paths and graph details can still reveal
application information.

## Validation

```sh
cargo fmt --all --check
cargo fmt --manifest-path test-programs/Cargo.toml --check
cargo nextest run --no-fail-fast -p hansei
```

Protocol tests exercise initialization, notifications, framing, recovery after
invalid requests, and size bounds. `test_mcp_core_session` launches the actual
server against a newly captured fixture core, compares its data with the CLI,
and tests all six tools, pagination, error recovery and repeated queries.
