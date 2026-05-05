# AI Agent Context

This directory is optimized for AI agents, not human onboarding.

Rules:

- Follow `token-policy.md`.
- Read only the smallest file needed.
- Prefer these files before scanning the whole repo.
- Treat human docs as explanatory; treat AI docs as compact contracts.
- Do not mirror bilingual content here.
- Verify code truth before editing when a contract references a source path.

Suggested load order:

1. `token-policy.md` for mandatory context rules.
2. `task-router.md` to choose the smallest context set.
3. `repo-map.md` for ownership and entrypoints.
4. `contracts.md` for stable manifest, registry, logging, kernel API, and path rules.
5. `commands.md` only when running checks/builds.
6. `workflows.md` only when implementing a known task type.

Primary human docs:

- `docs/index.md`
- `docs/getting-started.md`
- `docs/execution-flow.md`
- `docs/structure.md`
- `docs/logging.md`

Token policy:

- Canonical policy lives in `token-policy.md`.
- Start with `task-router.md`, then load only the focused docs/source files it names.
- Never inspect `build/`, `target/`, `logs/`, or `vendor/` unless the task exception requires it.
