# RustOS Documentation

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS is a Rust-built **hybrid microkernel** with native binary
compatibility for Linux ELF and Windows PE programs. The kernel keeps only
the mechanisms that require ring0 — trap entry, paging, scheduling,
IRQ/MMIO/DMA bridges, and a small set of gated brokers — and pushes
everything else into userspace services. A built-in Wayland compositor in
`uiserver` provides the desktop surface for both native and PE clients.

This book is split by audience:

- **Start:** build, run, debug, and follow the OS execution flow.
- **Architecture:** how the microkernel and services fit together, the
  Wayland UI server, the compat path, and repo structure/logging rules.
- **OS Developer APIs:** kernel crate APIs, `cargo xtask`, the
  `RUSTOS.package.toml` manifest, and runtime control.
- **Guides:** end-to-end recipes for adding services, apps, and drivers.
- **Reference:** stable paths and environment variables.
- **AI Agent Reference:** compact machine-oriented context under
  `docs/ai/`.

Recommended reading order:

1. [Getting Started](getting-started.md)
2. [Execution Flow](execution-flow.md)
3. [Microkernel Overview](architecture/microkernel-overview.md)
4. [Userspace Services](architecture/services.md)
5. [Userspace Compatibility](architecture/userspace-compat.md)
6. [UI Server & Wayland](architecture/ui-server.md)
7. [Structure Guide](structure.md)
8. [Kernel API](api/kernel.md)
9. [xtask API](api/xtask.md)
10. [Package Manifest API](api/package-manifest.md)
11. [Logging Guide](logging.md)
12. [Fault Injection](fault-injection.md)

<a id="korean"></a>

## 한국어

RustOS는 Rust로 작성된 **hybrid microkernel**이며, Linux ELF와 Windows PE
실행 파일에 대해 native binary compatibility를 목표로 합니다. 커널은
ring0가 반드시 필요한 mechanism — trap entry, paging, scheduling,
IRQ/MMIO/DMA bridge, 그리고 소수의 gated broker — 만 남기고 나머지는
모두 userspace service로 옮깁니다. `uiserver`에 내장된 Wayland compositor
가 native client와 PE client 모두에게 desktop surface를 제공합니다.

이 문서는 대상별로 나뉩니다.

- **Start:** build, run, debug, 그리고 OS 실행 흐름.
- **Architecture:** microkernel과 service들이 어떻게 맞물리는지, Wayland
  UI server, 호환 경로, repo 구조와 logging 규칙.
- **OS Developer APIs:** kernel crate API, `cargo xtask`,
  `RUSTOS.package.toml` manifest, runtime control.
- **Guides:** service / app / driver를 추가하는 끝-끝 절차.
- **Reference:** 안정적인 path와 environment variable.
- **AI Agent Reference:** `docs/ai/` 아래의 짧은 machine-oriented
  context.

추천 읽기 순서:

1. [시작하기](getting-started.md)
2. [실행 흐름](execution-flow.md)
3. [Microkernel Overview](architecture/microkernel-overview.md)
4. [Userspace Services](architecture/services.md)
5. [Userspace Compatibility](architecture/userspace-compat.md)
6. [UI Server & Wayland](architecture/ui-server.md)
7. [Structure Guide](structure.md)
8. [Kernel API](api/kernel.md)
9. [xtask API](api/xtask.md)
10. [Package Manifest API](api/package-manifest.md)
11. [로깅 가이드](logging.md)
12. [Fault Injection](fault-injection.md)
