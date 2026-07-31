# 릴리스·업데이트 안전성

- update package는 content hash, signature, version, dependency, rollback index를 포함한다.
- 부트로더·kernel·userspace service의 호환 범위를 manifest로 검사한다.
- write 순서는 전원 손실 후 이전 또는 새 버전 중 하나가 완전하게 부팅되도록 한다.
- health check가 단순 프로세스 생존이 아니라 storage/network/IPC 핵심 기능을 확인한다.
- 새 버전이 persistent state를 마이그레이션하기 전 snapshot/backup을 만든다.
- downgrade 불가 데이터 포맷은 rollout 전 명시적 gate를 둔다.
- security rollback protection과 현장 emergency rollback을 별도 authorization으로 해결한다.
- crash loop threshold 후 recovery image 또는 safe service set으로 부팅한다.
- release artifact에 source revision, toolchain, config, SBOM, test report, known issues를 묶는다.
