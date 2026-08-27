---
name: rustos-code-editing
description: Plan, inspect, refactor, and verify RustOS source changes with a Serena-first workflow backed by ast-grep structure checks and CodeGraph impact analysis. Use for any kernel, service, driver, compatibility, library, or tooling source edit; skip documentation-only changes.
metadata:
  short-description: Serena-first OS code editing with AST and graph checks
---

# RustOS Code Editing

Use this skill for every change to Rust, C/C++, shell, Python, or other
executable project source. It is intentionally strict because RustOS changes
cross ring0/ring3, IPC, ABI, lifecycle, and hardware boundaries.

## Hard preflight gate

Before changing source, verify all three project MCP servers with a focused
probe:

1. Serena: activate `/home/hongii2/rustos`, then use symbol or reference
   lookup on the target area.
2. ast-grep MCP: list its tools and run a Rust structural query or rule probe.
3. CodeGraph: list its tools and obtain a focused module, caller/callee,
   dependency, or impact result.

If any server or focused tool fails, stop source editing immediately. Report
the server, tool, error, and last successful probe. Do not fall back to local
`rg`, raw file rewrites, or an unverified text edit to keep the change moving.
Documentation and agent-infrastructure edits are outside this source gate.

## Serena is the primary editor

Use Serena for the semantic loop:

1. Search symbols and references before opening bodies.
2. Read only the exact symbol/range needed.
3. Use `rename_symbol` or `safe_delete_symbol` for reference-aware refactors.
4. Use `replace_symbol_body`, `insert_*_symbol`, or narrowly scoped
   `replace_content` for edits.
5. Re-query the changed symbol and its references after the edit.

Do not use ast-grep or CodeGraph as a substitute for Serena's edit and
reference-aware operations. Their roles are structural matching and impact
evidence.

## Structural and graph checks

Use ast-grep to express syntax, not formatting: `dump_syntax_tree` when a
pattern is uncertain, `test_match_code_rule` for a YAML rule, `find_code` for
a small pattern, and `find_code_by_rule` for relational rules. Test a rule on a
small example before applying it to RustOS.

Use CodeGraph before edits that change symbols, signatures, ownership, IPC, or
module boundaries. Prefer `codegraph_get_edit_context` for a target location,
then `codegraph_analyze_impact`, `codegraph_get_callers`,
`codegraph_get_callees`, and `codegraph_get_dependency_graph` as needed. The
project server runs graph-only to keep indexing bounded; treat unavailable
semantic/embedding features as a tool failure, not as permission to guess.

Detailed tool names and argument shapes are in
`references/three-tool-contract.md`; read it when a query needs exact syntax.

## OS correctness pass

Before finalizing, name the target's execution context and trust boundary.
Check lock order, IRQ/preemption state, blocking and allocation rules, user
copy/page faults, object publication and teardown, capability rights and
generation, ABI or wire-format impact, cancellation/timeout behavior, and
partial-initialization/hot-unplug paths. Keep policy in the owning user
service; do not move it back into ring0.

Keep one logical change per patch. Do not widen a fallback or preserve a
legacy route merely to make an incomplete primary route pass.

## Validation and handoff

After edits, run `cargo xtask dev-plan` and execute the selected `now` checks;
run the relevant `stable-batch` commands once the change set settles. Use the
repository's focused AI contracts and service/kernel instructions for the
exact lane. For DVM work, run `make -C driver-domains/linux build-plan` before
the integration build. Never use `--no-verify`, `--no-gpg-sign`, `clean`, or
`distclean` as a shortcut.

Report changed paths, tool probes, validation commands, and any unverified
hardware or runtime boundary. Commit only when the user requests it.
