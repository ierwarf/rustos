# 질문별 참조 지도

| 질문 | 우선 파일 | 보조 파일 |
|---|---|---|
| 새 syscall을 추가해도 되는가 | `01_LINUX/docs_abi/adding-syscalls.rst` | `02_DUAL_ABI/DUAL_ABI_DESIGN_GUIDE_KO.md` |
| ioctl 설계 | `01_LINUX/docs_abi/ioctl-number.rst` | `07_ENGINEERING_PRACTICE/OS_COMMON_SENSE_CHECKLIST_KO.md` |
| SMP에서 간헐적 멈춤 | `01_LINUX/docs_concurrency/memory-barriers.txt` | `atomic_t.txt`, `locktypes.rst`, `whatisRCU.rst`, `kcsan.rst` |
| lock-free 큐 | `memory-barriers.txt`, `atomic_t.txt` | `05_VERIFICATION/VERIFICATION_LADDER_KO.md` |
| Linux ABI와 자체 ABI 공존 | `02_DUAL_ABI/DUAL_ABI_DESIGN_GUIDE_KO.md` | `FUCHSIA_STARNIX_NOTES_KO.md`, `FREEBSD_LINUXULATOR_NOTES_KO.md` |
| 마이크로커널 IPC 설계 | `03_MICROKERNELS/MICROKERNEL_COMPARISON_KO.md` | seL4 README/CAVEATS, Qubes qrexec internals |
| 드라이버를 userspace로 옮기기 | `03_MICROKERNELS/DRIVER_ISOLATION_GUIDE_KO.md` | `04_QUBES_XEN/QUBES_ARCHITECTURE_NOTES_KO.md` |
| Qubes식 격리 | `04_QUBES_XEN/qubes_docs/architecture.rst` | `qrexec.rst`, `qrexec-internals.rst`, `security-critical-code.rst` |
| 형식 검증 도입 | `05_VERIFICATION/VERIFICATION_LADDER_KO.md` | `tla_plus/README.md`, `sel4_l4v/README.md` |
| 커밋이 왜 병합되지 않는가 | `08_REVIEW_CASES/README.md` | 각 CSV와 `REVIEW_PATTERNS_KO.md` |
| 상용 릴리스 준비 | `07_ENGINEERING_PRACTICE/COMMERCIAL_OS_DEFINITION_OF_DONE_KO.md` | `RELEASE_AND_UPDATE_SAFETY_KO.md` |
| AI가 만든 대규모 패치 리뷰 | `07_ENGINEERING_PRACTICE/AI_GENERATED_PATCH_REVIEW_KO.md` | Linux submit checklist, review cases |
