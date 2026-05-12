# AI Agent Reference

This directory is optimized for AI agents, not human onboarding.

Rules:

- Follow `token-policy.md`.
- Use root `AGENTS.md`, `docs/ai-map.md`, and `task-router.md` as entrypoints.
- Read the smallest file/range needed before scanning the repo.
- Treat human docs as explanatory and AI docs as compact contracts.
- Verify code truth before editing when a contract references a source path.
- For OS hardening, prioritize high-risk boundaries over broad cleanup.
- For blocked debugging, stop and report the structural blocker instead of
  making speculative patches.

Stable cache prefix:

1. Root `AGENTS.md`.
2. `docs/ai-map.md`.
3. `token-policy.md`.
4. `task-router.md`.

Then append one focused AI doc selected by `task-router.md`.

Primary human docs:

- `docs/index.md`
- `docs/ai-map.md`
- `docs/getting-started.md`
- `docs/execution-flow.md`
- `docs/structure.md`
- `docs/logging.md`

Token policy:

- Canonical policy lives in `token-policy.md`; do not duplicate it here.
