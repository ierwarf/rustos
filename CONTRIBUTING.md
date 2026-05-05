# Contributing

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS is an experimental operating system workspace. Contributions should keep
the repository buildable, documented, and easy to inspect from GitHub.

Before opening a pull request:

1. Run `cargo fmt --all -- --check`.
2. Run `cargo xtask check`.
3. Run the host tests listed in `.github/workflows/rust.yml` when touching
   tools, libraries, storage, runtime control, or keyboard logic.
4. Run `mdbook build` when changing `docs/`, `README.md`, or `book.toml`.
5. Update `CHANGELOG.md` for user-visible, workflow-visible, or repository
   structure changes.
6. Update `docs/ai/` when a change affects commands, stable paths, manifests,
   runtime contracts, logging, or common agent workflows.

Pull request expectations:

- Keep changes scoped to one topic.
- Do not commit generated outputs from `build/`, `target/`, `logs/`, profiling
  files, debugger history, or local editor state.
- Link related docs when adding or changing OS developer APIs.
- Explain QEMU, firmware, or host-specific assumptions in the PR body.

<a id="korean"></a>

## 한국어

RustOS는 실험적인 운영체제 workspace입니다. 기여는 GitHub에서 보기 쉽고,
빌드 가능하며, 문서와 함께 유지되는 방향이어야 합니다.

Pull request를 열기 전:

1. `cargo fmt --all -- --check`를 실행합니다.
2. `cargo xtask check`를 실행합니다.
3. tool, library, storage, runtime control, keyboard logic을 수정했다면
   `.github/workflows/rust.yml`에 있는 host test를 실행합니다.
4. `docs/`, `README.md`, `book.toml`을 수정했다면 `mdbook build`를 실행합니다.
5. 사용자에게 보이는 변경, workflow 변경, repository 구조 변경은
   `CHANGELOG.md`에 기록합니다.
6. command, stable path, manifest, runtime contract, logging, agent workflow에
   영향이 있으면 `docs/ai/`도 갱신합니다.

Pull request 기준:

- 하나의 주제에 집중합니다.
- `build/`, `target/`, `logs/`, profiling file, debugger history, local editor
  state를 커밋하지 않습니다.
- OS developer API를 추가하거나 바꾸면 관련 문서를 연결합니다.
- QEMU, firmware, host-specific assumption은 PR 본문에 적습니다.
