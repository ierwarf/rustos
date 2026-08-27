# Three-tool contract

This project pins four local inputs in `.codex/config.toml`: Serena 1.6.0,
ast-grep CLI 0.45.2 plus the official `sg-mcp` server at commit
`149e20d47bb7125fb0c1451feea2f48a98742034`, and CodeGraph MCP 0.20.1. CodeGraph
is configured with `--graph-only` and excludes generated, vendored, and lock
file inputs.

## Serena MCP

Use `activate_project` first. Prefer `find_symbol`,
`find_referencing_symbols`, `get_symbols_overview`, and `read_file` with
focused ranges. For edits use `rename_symbol`, `safe_delete_symbol`,
`replace_symbol_body`, `insert_before_symbol`, `insert_after_symbol`, or
`replace_content`. Serena line numbers are 0-based.

## ast-grep MCP

The server exposes four tools:

- `dump_syntax_tree(code, language, format)` — inspect AST/CST or pattern shape.
- `test_match_code_rule(code, yaml)` — validate a rule against a small sample.
- `find_code(project_folder, pattern, language, max_results, output_format)` —
  search simple structural patterns.
- `find_code_by_rule(project_folder, yaml, max_results, output_format)` —
  search relational/composite YAML rules.

Use `language: "rust"` for RustOS. A no-match result is evidence of no match,
not a server failure; malformed rules, missing binaries, or MCP errors are
failures. Do not use a rewrite command for source edits in this workflow;
make the final edit through Serena after the structural result is understood.

## CodeGraph MCP

All tools are prefixed `codegraph_`. For a target symbol, use a `file://` URI
and a 0-based line. Prefer:

- `codegraph_get_edit_context({uri, line, maxTokens})` before modifying code;
- `codegraph_analyze_impact({uri, line, changeType})` before rename/delete or
  boundary changes;
- `codegraph_get_callers` / `codegraph_get_callees` for execution flow;
- `codegraph_get_dependency_graph({uri, direction, depth})` for module impact;
- `codegraph_get_module_summary({path})` for unfamiliar subsystem routing.

The configured graph-only server must still answer structural graph queries.
If a requested feature is unavailable or returns an MCP error, stop the source
edit and report the limitation. Never turn a partial graph into a claim that
the change has no callers or dependents.

## Evidence order

Serena identifies the authoritative symbol and owns the edit. ast-grep confirms
the syntax-wide shape. CodeGraph identifies callers, callees, dependencies,
and blast radius. Tests, hooks, and `cargo xtask dev-plan` remain the final
validation evidence; tool output alone is not proof of correctness.
