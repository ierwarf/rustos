# Xen 참조 경로

extended 수집 프로필에서 다음을 우선한다.

- `xen/include/public/`: guest/hypervisor ABI, event channels, grant tables
- `xen/common/event_channel.c`: notification lifecycle
- `xen/common/grant_table.c`: shared page authorization and revoke
- `xen/common/domain.c`: domain lifecycle
- `xen/common/schedule.c`: scheduler interfaces
- `xen/arch/x86/hvm/`: HVM virtualization
- `xen/arch/x86/pv/`: PV compatibility
- `docs/`: hypercall ABI, security process, design notes

AI가 특히 확인할 것:

- grant revoke와 in-flight DMA/IO의 관계
- event channel close와 pending notification
- domain destruction 중 shared mapping 정리
- toolstack 실패와 hypervisor object lifetime
- live migration 중 ABI/version compatibility
- speculative execution·IOMMU·interrupt remapping assumptions
