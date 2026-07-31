# Qubes OS·Xen 설계 노트

## 핵심 관점

Qubes는 “VM을 많이 띄우는 데스크톱”보다 **서로 다른 신뢰 수준의 작업을 분리하고, 통신을 정책화하는 시스템**으로 보는 편이 정확하다.

## 구성 요소별 질문

### dom0
- 네트워크와 일반 사용자 데이터를 직접 처리하지 않는가?
- 관리 GUI·qubesd·정책 엔진이 외부 입력을 받을 때 parser 경계는 어디인가?
- dom0 업데이트와 복구 경로가 독립적인가?

### qrexec
- service name과 argument를 분리해 검증하는가?
- source/destination qube를 신뢰할 수 없는 입력으로 다루는가?
- 정책 매칭이 canonicalization 이전·이후 어느 시점인가?
- stdin/stdout 방향, EOF, timeout, 취소를 명확히 하는가?
- RPC service가 파일 경로나 shell을 다시 해석하지 않는가?

### GUI
- 창 content, title, clipboard, drag-and-drop, input focus를 별도 채널로 본다.
- 보안 레이블은 untrusted window가 위조할 수 없는 경로에서 그린다.
- clipboard와 file copy는 명시적 사용자 동작과 정책을 거친다.

### storage
- template root와 private volume의 의미를 분리한다.
- snapshot/reflink/COW의 flush·crash consistency를 확인한다.
- disposable 종료와 volume cleanup 사이 race를 테스트한다.

### network
- NetVM/FirewallVM이 compromised될 때 가능한 범위를 모델링한다.
- DNS, time, update metadata가 어느 qube를 거치는지 추적한다.
- 네트워크 없는 qube의 covert channel을 별도 위협으로 본다.

## 자체 OS에 적용할 수 있는 패턴

- service call마다 source identity와 destination policy를 함께 전달
- policy decision과 actual connection 사이 TOCTOU 방지
- 고위험 parser는 별도 service/VM으로 격리
- disposable service instance와 generation-bound handle
- UI에 신뢰 경계를 OS가 직접 표시
- storage/network/driver host를 교체 가능한 compartment로 구성

원문은 `qubes_docs/architecture.rst`, `qrexec.rst`, `qrexec-internals.rst`, `admin-api.rst`, `security-critical-code.rst`를 참고한다.
