// SPDX-License-Identifier: GPL-2.0-only
/*
 * Fixed RustOS network-DVM ivshmem adapter.
 *
 * The adapter admits the fixed RSDVMNT1 WB aperture. In MSI-X mode UIO
 * exposes interrupt counts and a fixed peer-0/vector-0 kick, not BAR0.
 */

#include <linux/build_bug.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/pci.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uio_driver.h>
#include <linux/unaligned.h>

#define RUSTOS_IVSHMEM_VENDOR_ID 0x1af4
#define RUSTOS_IVSHMEM_DEVICE_ID 0x1110
#define RUSTOS_IVSHMEM_SHARED_BAR 2
#define RUSTOS_IVSHMEM_REGISTERS_BAR 0
#define RUSTOS_IVSHMEM_POSITION_OFFSET 8U
#define RUSTOS_IVSHMEM_DOORBELL_OFFSET 12U
#define RUSTOS_DVM_NET_UIO_NAME "rustos-dvm-net"

#define RUSTOS_DVM_NET_VERSION 1U
#define RUSTOS_DVM_NET_HEADER_BYTES 4096U
#define RUSTOS_DVM_NET_RECORD_BYTES 64U
#define RUSTOS_DVM_NET_SLOT_COUNT 64U
#define RUSTOS_DVM_NET_SLOT_BYTES 2048U
#define RUSTOS_DVM_NET_MTU 1514U
#define RUSTOS_DVM_NET_APERTURE_BYTES (512ULL * 1024ULL)
#define RUSTOS_DVM_NET_FLAG_READY BIT(0)
#define RUSTOS_DVM_NET_FLAG_DVM_READY BIT(1)
#define RUSTOS_DVM_NET_KNOWN_FLAGS \
	(RUSTOS_DVM_NET_FLAG_READY | RUSTOS_DVM_NET_FLAG_DVM_READY)

static_assert(RUSTOS_DVM_NET_HEADER_BYTES +
	      2ULL * RUSTOS_DVM_NET_SLOT_COUNT * RUSTOS_DVM_NET_SLOT_BYTES <=
	      RUSTOS_DVM_NET_APERTURE_BYTES);
static_assert((RUSTOS_DVM_NET_APERTURE_BYTES &
	      (RUSTOS_DVM_NET_APERTURE_BYTES - 1ULL)) == 0ULL);

static const u8 rustos_dvm_net_magic[8] = {
	'R', 'S', 'D', 'V', 'M', 'N', 'T', '1'
};

struct rustos_dvm_net_uio {
	struct uio_info uio;
	void __iomem *doorbell;
	resource_size_t shared_start;
	resource_size_t shared_bytes;
};

static bool rustos_dvm_net_cursor_pair_valid(u32 producer, u32 consumer)
{
	return producer - consumer <= RUSTOS_DVM_NET_SLOT_COUNT;
}

static int rustos_dvm_net_validate_aperture(struct pci_dev *pdev)
{
	struct resource *shared =
		&pdev->resource[RUSTOS_IVSHMEM_SHARED_BAR];
	void __iomem *mapped;
	u8 bytes[RUSTOS_DVM_NET_RECORD_BYTES];
	u32 flags;
	u32 tx_producer;
	u32 tx_consumer;
	u32 rx_producer;
	u32 rx_consumer;
	int result = -ENODEV;

	if (!(shared->flags & IORESOURCE_MEM) ||
	    !(shared->flags & IORESOURCE_PREFETCH) ||
	    resource_size(shared) != RUSTOS_DVM_NET_APERTURE_BYTES)
		return -ENODEV;

	/*
	 * Every retained alias is WB.  Header discovery must use the same type;
	 * pci_iomap() could create a transient UC alias on x86.
	 */
	mapped = ioremap_cache(shared->start, sizeof(bytes));
	if (!mapped)
		return -ENOMEM;
	memcpy_fromio(bytes, mapped, sizeof(bytes));

	flags = get_unaligned_le32(bytes + 36U);
	tx_producer = get_unaligned_le32(bytes + 40U);
	tx_consumer = get_unaligned_le32(bytes + 44U);
	rx_producer = get_unaligned_le32(bytes + 48U);
	rx_consumer = get_unaligned_le32(bytes + 52U);
	if (!memcmp(bytes, rustos_dvm_net_magic,
		    sizeof(rustos_dvm_net_magic)) &&
	    get_unaligned_le32(bytes + 8U) == RUSTOS_DVM_NET_VERSION &&
	    get_unaligned_le32(bytes + 12U) == RUSTOS_DVM_NET_HEADER_BYTES &&
	    get_unaligned_le64(bytes + 16U) ==
		    RUSTOS_DVM_NET_APERTURE_BYTES &&
	    get_unaligned_le32(bytes + 24U) == RUSTOS_DVM_NET_SLOT_COUNT &&
	    get_unaligned_le32(bytes + 28U) == RUSTOS_DVM_NET_SLOT_BYTES &&
	    get_unaligned_le32(bytes + 32U) == RUSTOS_DVM_NET_MTU &&
	    (flags & RUSTOS_DVM_NET_FLAG_READY) &&
	    !(flags & ~RUSTOS_DVM_NET_KNOWN_FLAGS) &&
	    get_unaligned_le64(bytes + 56U) != 0 &&
	    rustos_dvm_net_cursor_pair_valid(tx_producer, tx_consumer) &&
	    rustos_dvm_net_cursor_pair_valid(rx_producer, rx_consumer))
		result = 0;

	iounmap(mapped);
	return result;
}

static int rustos_dvm_net_mmap(struct uio_info *info,
			       struct vm_area_struct *vma)
{
	struct rustos_dvm_net_uio *state = info->priv;
	unsigned long mapped_bytes = vma->vm_end - vma->vm_start;

	if (!state || vma->vm_pgoff != 0 || !mapped_bytes ||
	    mapped_bytes != state->shared_bytes)
		return -EINVAL;
	vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
	/*
	 * BAR2 is coherent shared RAM.  Clear PAT cache bits so Linux and RustOS
	 * both use WB for the atomic cursor words and payload slots.
	 */
	vma->vm_page_prot = __pgprot(pgprot_val(vma->vm_page_prot) &
				     ~_PAGE_CACHE_MASK);
	return remap_pfn_range(vma, vma->vm_start,
			       state->shared_start >> PAGE_SHIFT, mapped_bytes,
			       vma->vm_page_prot);
}

static irqreturn_t rustos_dvm_net_irq(int irq, struct uio_info *info)
{
	return IRQ_HANDLED;
}

static int rustos_dvm_net_kick(struct uio_info *info, s32 value)
{
	struct rustos_dvm_net_uio *state = info->priv;

	if (!state || value != 1)
		return -EINVAL;
	/* Order Release cursor publication before the fixed peer-0 nudge. */
	wmb();
	writel(0, state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
	return 0;
}

static void rustos_dvm_net_free_vectors(void *data)
{
	pci_free_irq_vectors(data);
}

static int rustos_dvm_net_init_notification(struct pci_dev *pdev,
					    struct rustos_dvm_net_uio *state)
{
	struct resource *registers = &pdev->resource[RUSTOS_IVSHMEM_REGISTERS_BAR];
	int result;

	state->uio.version = "1";
	state->uio.irq = UIO_IRQ_NONE;
	if (!pdev->msix_cap)
		return 0;
	if (!(registers->flags & IORESOURCE_MEM) ||
	    resource_size(registers) < RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32) ||
	    pci_msix_vec_count(pdev) != 1)
		return -ENODEV;
	state->doorbell = pcim_iomap(pdev, RUSTOS_IVSHMEM_REGISTERS_BAR,
				    RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32));
	if (!state->doorbell)
		return -ENOMEM;
	if (readl(state->doorbell + RUSTOS_IVSHMEM_POSITION_OFFSET) != 1)
		return -ENODEV;
	result = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_MSIX);
	if (result < 0)
		return result;
	result = devm_add_action_or_reset(&pdev->dev, rustos_dvm_net_free_vectors, pdev);
	if (result)
		return result;
	state->uio.version = "2";
	state->uio.irq = pci_irq_vector(pdev, 0);
	state->uio.handler = rustos_dvm_net_irq;
	state->uio.irqcontrol = rustos_dvm_net_kick;
	return 0;
}

static int rustos_dvm_net_probe(struct pci_dev *pdev,
				const struct pci_device_id *id)
{
	struct rustos_dvm_net_uio *state;
	struct resource *shared = &pdev->resource[RUSTOS_IVSHMEM_SHARED_BAR];
	int result;

	result = pcim_enable_device(pdev);
	if (result)
		return result;
	result = rustos_dvm_net_validate_aperture(pdev);
	if (result)
		return result;
	state = devm_kzalloc(&pdev->dev, sizeof(*state), GFP_KERNEL);
	if (!state)
		return -ENOMEM;
	result = rustos_dvm_net_init_notification(pdev, state);
	if (result)
		return result;
	state->shared_start = shared->start;
	state->shared_bytes = resource_size(shared);
	state->uio.name = RUSTOS_DVM_NET_UIO_NAME;
	state->uio.mmap = rustos_dvm_net_mmap;
	state->uio.mem[0].name = "rustos-dvm-net-aperture-wb";
	state->uio.mem[0].memtype = UIO_MEM_PHYS;
	state->uio.mem[0].addr = shared->start;
	state->uio.mem[0].size = resource_size(shared);
	state->uio.priv = state;
	pci_set_drvdata(pdev, state);

	result = devm_uio_register_device(&pdev->dev, &state->uio);
	if (!result)
		dev_info(&pdev->dev,
			 "RustOS network UIO bound: MSI-X=%ld WB BAR2=%pa+%pa\n",
			 (long)state->uio.irq, &shared->start, &state->shared_bytes);
	return result;
}

static const struct pci_device_id rustos_dvm_net_ids[] = {
	{ PCI_DEVICE(RUSTOS_IVSHMEM_VENDOR_ID, RUSTOS_IVSHMEM_DEVICE_ID) },
	{ }
};
MODULE_DEVICE_TABLE(pci, rustos_dvm_net_ids);

static struct pci_driver rustos_dvm_net_driver = {
	.name = "rustos-dvm-net-uio",
	.id_table = rustos_dvm_net_ids,
	.probe = rustos_dvm_net_probe,
};
module_pci_driver(rustos_dvm_net_driver);

MODULE_AUTHOR("RustOS");
MODULE_DESCRIPTION("RustOS fixed network-DVM ivshmem WB adapter");
MODULE_LICENSE("GPL");
MODULE_VERSION("1");
