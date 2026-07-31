# 기타 호환 계층 비교

| 프로젝트 | 주요 접근 | 참고할 강점 | 주의점 |
|---|---|---|---|
| gVisor | 사용자 공간 Sentry가 Linux syscall 의미 구현 | 격리와 syscall surface 관리 | host syscall과 Linux 의미의 차이, 성능 |
| Wine | Windows user-mode API/ABI 재구현 | 방대한 API 호환 테스트, WoW64 | kernel driver/anti-cheat/저수준 장치 한계 |
| ReactOS | NT 호환 운영체제 구현 | NT 객체·I/O·Win32 subsystem 구조 | 완전 호환의 장기 비용 |
| illumos lx brand | zone personality로 Linux ABI 제공 | 격리와 personality 결합 | 지원 syscall 범위·배포판 기대치 |
| Linux ia32/x32 compat | 동일 커널의 다중 word-size ABI | compat 구조체·syscall table | time32, pointer width, ioctl 구조체 |
| WSL2 | 경량 VM 안 Linux kernel | 높은 Linux 호환성 | VM 경계, 파일·네트워크 통합 의미 |

`tools/fetch_full_reference_pack.sh`의 extended 프로필이 관련 저장소와 경로를 가져온다.
