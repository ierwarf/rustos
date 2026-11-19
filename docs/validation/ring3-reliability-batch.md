# Ring 3 and non-core Ring 0 reliability batch

[English](#english) · [한국어](#korean)

<a id="english"></a>

## English

This is a validation record for [`615b115c`](https://github.com/ierwarf/rustos/commit/615b115c), not a claim about every later checkout or a release qualification. The batch changed RustOS services, shared libraries, compatibility code and kernel I/O bridges; it did not change the Linux DVM appliance.

| Area | What changed |
| --- | --- |
| Files and storage | TmpFs directory path reconstruction no longer scans each parent directory. Failed shared writes roll back file growth; directory rename preserves descendant shared-backing identity. FAT fixes cover directory-cycle checks, short writes when full, invalid timestamps and the unmount barrier. |
| Sockets | TCP reports a pending reset through `SO_ERROR` and returns the completed result on the first retry of a nonblocking connect. The batch also fixes Unix socket shutdown/readiness handling and avoids scanning the live provider reservation set for refund work. |
| Process and memory services | Waiting for loaderd or another dependency no longer consumes a dependent service's restart attempts. Loader reads retry short positive reads. Pagerd backing revocation removes the hash index before clearing a loading slot. |
| UI and input | Recreated Wayland toplevels do not inherit the previous toplevel's title, decoration preference or saved maximize state. Input overflow sheds lower-priority events before releases. |
| Non-core kernel paths | Pipe reads resample queued data after a blocking wakeup. Win32 virtual-memory operations reject invalid release/decommit/unmap requests and several filesystem errors now map to specific NTSTATUS values. |

### Verification at that revision

- `cargo xtask check` passed. Host runs reported **1,862 passed, 3 ignored** across 33 harness results; native procd/rootd tests used their `host-test` features.
- `bash formal/verify-all.sh --profile pr` passed and sealed **45 artifacts**. Formal evidence is tied to the exact source tree.
- One-vCPU KVM smoke with `--local-fd-probe --dvm-block-shmem` exited 0 and verified all ten pipe/eventfd cases. Eight-vCPU KVM smoke using the PR-sealed tree with GUI, block, input and offline network exercise also exited 0. Display readiness exceeded its advisory deadline in both profiles; this is not a timing qualification, physical-device test or storage-durability proof.
- In a pinned, optimized x86-64 **host microbenchmark** of 8–31-byte copies, using 15 samples per implementation, the median changed from **10.04 to 4.68 ns** for disjoint `memcpy` and **11.33 to 6.67 ns** for overlapping `memmove` (2.15× / 1.70×). This is not guest or end-to-end throughput evidence.

### Gaps at `615b115c`

- `getdents64` uses position-based cookies: deleting an earlier entry during enumeration can cause a later entry to be skipped.
- Overlong normalized paths can still return `EINVAL` instead of Linux `ENAMETOOLONG`.
- A failed second FAT mount can leave `/data` absent without a mount failure at the top level; node-backed directory listings do not all merge synthetic mount children.
- This batch did not rerun the Chrome compatibility probe or prove Chrome startup. See [Linux compatibility](../architecture/userspace-compat.md) and the [dated Chrome probe results](../ai/chrome-gap-fs-net.md#acceptance-not-yet-complete).

<a id="korean"></a>

## 한국어

이 문서는 [`615b115c`](https://github.com/ierwarf/rustos/commit/615b115c)의 검증 기록입니다. 이후 체크아웃 전체나 릴리스 적격성을 보증하지 않습니다. RustOS 서비스, 공용 라이브러리, 호환성 코드와 커널 I/O 브리지를 수정했으며 Linux DVM 어플라이언스는 변경하지 않았습니다.

| 영역 | 변경 사항 |
| --- | --- |
| 파일·저장소 | TmpFs 경로 재구성에서 부모 디렉터리의 항목을 매번 훑지 않습니다. 공유 파일 쓰기가 실패하면 파일 확장을 되돌리고, 디렉터리 이름을 바꿔도 하위 파일의 공유 매핑을 유지합니다. FAT의 디렉터리 순환 검사, 공간 부족 시 짧은 쓰기, 잘못된 타임스탬프, 마운트 해제 장벽을 보완했습니다. |
| 소켓 | TCP `SO_ERROR`가 대기 중인 연결 재설정을 반환하고, 완료된 비동기 연결은 첫 `connect` 재호출에서 결과를 돌려줍니다. Unix 소켓의 종료·준비 상태 처리와 네트워크 자원 반환 큐도 개선했습니다. |
| 프로세스·메모리 | loaderd 등 의존 서비스가 준비되기를 기다리는 동안 재시작 횟수를 차감하지 않습니다. 로더는 짧은 읽기를 이어서 처리하고, pagerd는 로딩 슬롯을 비우기 전에 해시 인덱스를 제거합니다. |
| UI·입력 | Wayland 창을 다시 만들 때 이전 제목, 장식 설정, 최대화 전 상태를 이어받지 않습니다. 입력 큐가 가득 차면 해제 이벤트보다 우선순위가 낮은 이벤트부터 버립니다. |
| 비코어 커널 | 블로킹 파이프 읽기는 깨어난 뒤 대기 바이트 수를 다시 확인합니다. Win32 메모리 해제 요청의 범위·종류 검사와 파일 오류의 NTSTATUS 매핑을 보완했습니다. |

### 해당 커밋의 검증

- `cargo xtask check` 통과. 호스트 테스트 **1,862개 통과, 3개 제외**(33개 테스트 실행 결과). procd/rootd 네이티브 테스트에는 `host-test` 기능을 사용했습니다.
- formal PR 게이트 통과 및 **증거 45개 봉인**. 봉인은 검증한 소스 트리에만 유효합니다.
- 1 vCPU KVM에서 파이프·eventfd 10개 사례, PR 봉인 후 8 vCPU KVM에서 GUI·블록·입력·오프라인 네트워크 경로를 확인했습니다. 두 부팅 모두 종료 코드 0이지만 화면 준비는 권고 시간을 넘겼습니다. 물리 장치나 저장 내구성 또는 성능 적격성 검증은 아닙니다.
- 8–31바이트 x86-64 **호스트 마이크로벤치마크**(구현당 15회)의 중앙값은 `memcpy` **10.04 → 4.68 ns**, 겹치는 `memmove` **11.33 → 6.67 ns**였습니다. 게스트 또는 전체 시스템 성능 수치는 아닙니다.

### `615b115c` 시점의 한계

- 디렉터리 열거 도중 앞 항목을 지우면 위치 기반 `getdents64` 쿠키가 뒤 항목을 건너뛸 수 있습니다.
- 정규화된 경로가 너무 길면 Linux의 `ENAMETOOLONG` 대신 `EINVAL`을 반환하는 경로가 남아 있습니다.
- 두 번째 FAT 파티션의 마운트 실패가 상위 호출에 드러나지 않을 수 있으며, 일부 디렉터리 열거 경로는 `/data` 같은 마운트 지점을 목록에 합치지 않습니다.
- 이번 배치에서 Chrome 호환성 프로브는 다시 실행하지 않았습니다. 자세한 범위와 과거 결과는 [Linux 호환성](../architecture/userspace-compat.md) 및 [Chrome 프로브 기록](../ai/chrome-gap-fs-net.md#acceptance-not-yet-complete)을 참고하세요.
