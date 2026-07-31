# Linux mainline 반영 사례 읽기

이 표본은 2026-07-30~31의 torvalds/linux mainline 커밋에서 추렸다. 커밋 자체가 병합 결과이고, 관련 `Link:` 또는 merge message를 따라가면 mailing-list patch series와 maintainer pull 흐름을 볼 수 있다.

## 1. Subsystem pull

`Merge tag ...` 커밋은 maintainer가 자신의 tree에서 검토·테스트한 묶음을 Linus에게 signed tag/pull request 형태로 올리는 전형적 흐름이다. merge message에는 사용자 영향, regression 여부, 대표 수정과 contributor가 요약된다.

읽을 점:
- 왜 merge window가 아닌 rc 단계에서 들어오는가
- fix-only 범위가 지켜졌는가
- 몇 개의 독립 patch가 한 pull로 묶였는가
- stable backport가 필요한가

## 2. 직접 bug-fix commit

### qede self-deadlock
내부 mutex를 잡은 채 callback이 같은 mutex를 다시 잡는 경로를 stack trace로 증명하고, sync를 lock 밖으로 옮긴다. 단순 lock 삭제가 아니라 RTNL 요구와 기존 gating을 보존한다.

### Open vSwitch UAF
객체를 다른 CPU에 publish한 뒤 함수가 실패할 수 있었고, caller는 객체가 공개되지 않았다고 가정해 즉시 free했다. 해결은 failure point가 모두 지난 뒤 publish하는 것이다. 이는 OS 객체 lifecycle의 대표 패턴이다.

### PTP interrupt storm
unbind·ioctl·IRQ enable 순서가 경쟁해 stale interrupt가 재bind 뒤 무한 storm을 만들었다. IRQF_NO_AUTOEN, disable_irq, unregister 순서, IRQ_NONE 반환까지 여러 방어를 결합한다.

### MANA error propagation
하위 계층 실제 오류를 NULL과 -ENOMEM으로 뭉개던 API를 ERR_PTR로 바꿔 원인 코드를 보존한다. 상용 OS의 observability와 recovery에 중요하다.

### Realtek mutex series
같은 종류의 lifecycle fix라도 도입 commit이 다른 lock마다 patch를 나눠 stable backport와 bisect를 쉽게 한다. “반복이니 한 패치”보다 history와 backport 단위를 우선한 사례다.

## 적용 질문

- commit message만으로 문제와 영향이 재현 가능한가?
- `Fixes:`, `Link:`, `Reviewed-by:`, `Cc: stable`가 적절한가?
- object publish/free, lock ordering, teardown ordering 중 어느 invariant를 고쳤는가?
- patch가 backport 가능한 최소 범위인가?
- merge commit이 subsystem risk를 충분히 요약하는가?
