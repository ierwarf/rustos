# 코어 외 경로 벤치 묶음 (구축 중)

전체 목표/남은 검증은 [진행표](core-paths-progress.md)에 유지한다. 이 문서는
측정 인터페이스이지 최적화 완료 선언이 아니다. 코어 IPC 자체는 최적화 대상에서
제외하며 상태 변화 이후 IPC 비용이 영구 증가하는지는 계속 검사한다.

## 실행

```sh
cargo xtask bench --suite core_syscalls --rustos-vcpus 1
cargo xtask bench --suite core_memory --rustos-vcpus 1
cargo xtask bench --suite core_lifecycle --rustos-vcpus 1
cargo xtask bench --suite core_data --rustos-vcpus 1
cargo xtask bench --suite core_scheduler --rustos-vcpus 8
cargo xtask bench --suite core_state --rustos-vcpus 8
cargo xtask bench --suite core_gui --rustos-vcpus 8

# 같은 ELF, 같은 호스트에서 별도 Linux KVM 게스트 실행
cargo xtask bench --suite core_syscalls --linux-guest --rustos-vcpus 1
# 위 명령이 출력한 guest.log 경로 사용
cargo xtask bench --suite core_syscalls --rustos-vcpus 1 \
  --linux-baseline build/bench-linux/<private-run>/guest.log
```

`--suite`는 `--isolate-probe`의 별칭이다. 각 묶음은 1/8 vCPU를 별도로
실행한다. 기존 단일 프로브와 shipping IPC 최대값 게이트의 시간 제한은
변하지 않는다. **여러 경로 수집 전용** 묶음만 120초로 제한되며, 이 시간 안의
UI 준비를 제품 부팅 예산 통과로 해석하지 않는다.

벤치 빌드는 기존 signed xtask 경로를 사용한다. RustOS 원시 로그·실제 app/kernel/UI
ELF·DVM serial 로그·소스 해시·토폴로지·프로파일 플래그는 `build/bench-core/`의 고유 private
디렉터리에 남긴다. 실행 중 소스나 ELF 변경, linked/staged 불일치는 거부한다.
Linux 수집기는 기존 DVM 커널을 읽기만 하고 작은 별도 initramfs를 생성한다.
DVM rootfs나 서비스를 수정하지 않으며 새 도구를 설치하지 않는다. 동일 app ELF,
해결된 glibc 라이브러리, PID1, 커널 해시, QEMU 명령과 원시 로그는
`build/bench-linux/`에 남긴다. Linux는 Q35/KVM, `host,+x2apic,+invtsc`,
2 GiB 초기 RAM으로 실행한다. Linux 게스트는 최소 토폴로지이므로 RustOS의
살아 있는 데스크톱 부하까지 동일하다는 주장은 하지 않는다.

비율표는 동일 호스트 boot ID, app ELF, suite, vCPU 수와 CPUID p50의 3% 이내
일치를 요구한다. 또한 runner exit=0, timeout 없음, end 뒤의 정확한 PID1 성공
기록, source/ELF-bound collection seal이 필요하다. 실패한 아카이브를 나중에
비교 기준선으로 되살리지 않는다. 정규화/이상치 삭제는 하지 않는다. 과거 기준선이나 한 쌍의
비율만으로 개선을 주장하지 않고, 변경별 토글·재빌드·동시간대 ABAB가 별도로
필요하다. Linux 부팅/GUI의 동등 워크로드는 아직 이 수집기의 범위가 아니다.

## 프로브 의미

- `core_syscalls`: getuid, 8-byte memfd read/write, ELF openat/stat,
  pipe2 생성, 빈 epoll wait(0 timeout), waiter 없는 private futex wake.
  epoll/futex의 빈 상태를 실제 대기/깨우기 비용으로 인용하지 않는다.
- `core_memory`: 4 KiB mmap 예약, resident munmap, RW→R mprotect,
  익명 페이지 첫 쓰기 fault, **fork 밖에서 준비하고 child의 첫 store만 재는**
  `cow_first_write`. COW는 parent/child의 서로 다른 값과 child stamp를 검증한다.
- `core_lifecycle`: 기존 fork+wait, fork+exec+wait, thread+join,
  exec 교체, child first-turn, exit→reap, 전체 fork+COW 검증 트랜잭션.
  `fork_cow_private_write`와 COW 첫 store의 시간을 혼동하지 않는다.
- `core_data`: `/tmp`의 warm-cache regular-file 4K/64K/1M read/write,
  같은 파일 open+close, 128개 4 KiB 파일의 batch open+close,
  1-byte pipe/socket 왕복 ACK, 4K/64K/1M pipe/socket transfer+ACK.
  short read/write는 반복하여 지정 크기 전체를 전달한다. 모든 write는 이전과
  다른 payload를 사용하고 readback/consumer가 전체 bytes를 검사한다.
  저장장치 durability나 cold-cache 디스크 처리량은 측정하지 않는다.
  소켓은 AF_UNIX stream이다. 별도 AF_UNIX ACK 채널과 consumer 검증 비용도
  transfer 시간에 포함된다.
- `core_scheduler`: sched_yield, 실제 kernel waiter를 깨우는 private futex의
  wake→첫 계측 사용자 turn, 8-worker 동시 깨우기. wake 결과가 정확히 1이
  아니면 전체 프로브를 실패시키며 빠른 early wake를 샘플로 바꾸지 않는다.
  CPUID APIC 관찰은 end stamp **이후**여서 latency에 VM exit를 추가하지 않는다.
  관찰 직전 migration은 가능하므로 stamp와 CPU가 원자적으로 짝지어졌다고
  주장하지 않는다. histogram 합은 전체 timed sample 수와 반드시 일치한다.
- `core_state`: before 및 fork/exec/exit/signal/mmap/thread 후 null+실제 fused IPC.
  fork 단계는 child exit+wait를 포함하고, 별도 exit 단계는 nonzero terminal status의
  reap도 검증한다. 비교 전 child/thread를 정리해 runnable 부하를 늘리는 시험과
  영구 상태세 시험을 구별한다. Linux에는 RustOS IPC를 에뮬레이트하지 않는다.

`core_gui`는 별도 UI 측정이다. 기존 private KVM probe selector와 acceptance
profile을 함께 주입하고 DVM의 bounded uinput/evdev 부하를 켠다. GUI 측정과
IPC 최대값 게이트·Linux syscall 기준선을 혼합하지 않는다. 현재 Linux GUI
비교 경로는 없으며 다른 Linux 결과로 대체하지 않는다.

UI의 **cursor가 실제 변한 input drain**에서 시작해, 그 이후 최신 cursor scene을
포함한 accepted GPU frame의 render/present receipts를 모두 검증한 직후까지
잰다. warmup 16개 input-bearing frame의 completion을 받은 뒤 128개 frame의
전체 samples를 모은다. backpressure 중 여러 drain은 한 frame으로 coalesce될 수
있으며 가장 오래된 소비 시각, 마지막 소비 시각, drain/event 수와 최신 좌표를
보존한다. 중간 cursor 위치가 모두 화면에 표시됐다고 주장하지 않는다.

측정값은 monotonic microseconds이며 consume→submit과 submit→observed-completion으로
분해한다. completion polling 지연도 포함한다. slot/provider generation/content
 epoch는 실제 pending GPU frame에 붙이며 첫 frame의 동기 retirement 이전에
등록한다. CPU ioctl 반환, submission, key echo, physical scanout/photon 시간과
혼동하지 않는다. 취소·역전·중복 receipt, provider 변경, input drop/error,
잘린/누락된 row·sentinel은 수집 실패다. 고정 배열에 기록하고 128개 completion
후 동결한 뒤에만 이 벤치의 출력/포맷팅을 수행한다. 다른 서비스의 기존 로그
간섭은 그대로 raw 지연에 포함된다. 별도 ipcbench CPUID anchor는 isolate settle
후에 수집하므로 GUI 구간 전체의 host 안정성을 단독 증명하지 않는다.

일반 `cargo xtask bench`에도 상태 회귀 rail이 항상 포함된다. 모든 checkpoint와 20,000개씩의 IPC split이
존재해야 한다. null은 uaccess preflight의 fork/mmap/thread **이전**에 저장한
warm pristine 분포를, IPC는 before 분포를 기준으로 p50이 1.5배 이하여야 한다.
preflight 자체가 baseline을 부풀려 이후 상태세를 가리는 것도 거부한다.
이는 경로 전체의 p99 목표와 별개다. p99/p50 품질표는 raw 실패를 그대로 표시하며
수집 성공을 성능 목표 달성으로 바꾸지 않는다.

primitive는 2,000 warmup + 2,000 samples, bulk는 8 + 128, COW 첫 store는
8 + 128이다. 1-byte latency는 primitive 횟수를 쓴다. yield/CPUID/stamp는 기존
50,000 samples, IPC checkpoint는 기존 2,000 warmup + 20,000 samples를 유지한다.
8-worker balance는 8 warmup rounds + 256 measured rounds(2,048 samples)다.
정확한 lifecycle 횟수는 기존 함수와 공유 host 계약으로 검증한다.

ipcbench의 모든 TSC stamp는 `LFENCE; RDTSC; LFENCE`이며 비용을 빼지 않는다.
`core_gui`는 같은 UI thread의 monotonic Instant를 사용하며 TSC ticks로 위장하지 않는다. setup, allocation,
검증 및 teardown은 해당 연산 이름이 포함하는 경우를 제외하면 timed interval 밖이다.
묶음의 앱 출력은 전부 수집 후 내보낸다. 다른 서비스의 debugcon/IRQ/host 지연은
raw 꼬리에 남는다. ENOSYS·오류·short transcript·skip은 저렴한 성공 샘플이 아니라
**미지원/불완전 측정**이고, 묶음 전체의 collection COMPLETE를 거부한다.

## 아직 필요한 0단계 증거

- 모든 묶음의 실제 RustOS/Linux 1·8 vCPU 실행 및 비율표.
- 서비스별 boot와 GUI 입력→실제 프레임 완료의 반복 분포, 동등 Linux GUI 기준.
- shipping/프로파일 분리, ordered-TSC 비용 분해 및 ABAB 자동 기록.
- pipe2 미지원 등 드러난 coverage gap의 구현/소유권 결정과 후속 재측정.

새 ABI나 서비스 구조를 도입해야 하는 경우에만 설계 결정을 사용자에게 확인한다.
