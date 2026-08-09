// SPDX-License-Identifier: GPL-2.0-only
/*
 * Minimal RustOS storage-DVM ivshmem adapter.
 *
 * It binds only an exact RSDVMBL2 aperture, allocates one MSI-X vector, maps
 * only that fixed aperture into the relay, and converts a UIO irqcontrol write
 * into the fixed peer-0/vector-0 completion doorbell. Userspace never receives
 * raw BAR0 MMIO authority.
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
#define RUSTOS_IVSHMEM_REGISTERS_BAR 0
#define RUSTOS_IVSHMEM_SHARED_BAR 2
#define RUSTOS_IVSHMEM_DOORBELL_OFFSET 12U
#define RUSTOS_DVM_BLOCK_MSIX_VECTORS 1
#define RUSTOS_DVM_BLOCK_UIO_NAME "rustos-dvm-block"

#define RUSTOS_DVM_BLOCK_VERSION 2U
#define RUSTOS_DVM_BLOCK_HEADER_BYTES 4096U
#define RUSTOS_DVM_BLOCK_RECORD_BYTES 192U
#define RUSTOS_DVM_BLOCK_QUEUE_DEPTH 64U
#define RUSTOS_DVM_BLOCK_SLOT_BYTES (64U * 1024U)
#define RUSTOS_DVM_BLOCK_USED_BYTES \
	(RUSTOS_DVM_BLOCK_HEADER_BYTES + \
	 2ULL * RUSTOS_DVM_BLOCK_QUEUE_DEPTH * 64ULL + \
	 (u64)RUSTOS_DVM_BLOCK_QUEUE_DEPTH * RUSTOS_DVM_BLOCK_SLOT_BYTES)
#define RUSTOS_DVM_BLOCK_APERTURE_BYTES (8ULL * 1024ULL * 1024ULL)

static_assert(RUSTOS_DVM_BLOCK_USED_BYTES <=
	      RUSTOS_DVM_BLOCK_APERTURE_BYTES);
static_assert((RUSTOS_DVM_BLOCK_APERTURE_BYTES &
	      (RUSTOS_DVM_BLOCK_APERTURE_BYTES - 1ULL)) == 0ULL);
#define RUSTOS_DVM_BLOCK_KNOWN_FEATURES 0x1fULL
#define RUSTOS_DVM_BLOCK_REQUIRED_FEATURES 0x01ULL
#define RUSTOS_DVM_BLOCK_FLAG_RUSTOS_READY BIT(0)
#define RUSTOS_DVM_BLOCK_KNOWN_FLAGS (BIT(0) | BIT(1) | BIT(2))

static const u8 rustos_dvm_block_magic[8] = {
	'R', 'S', 'D', 'V', 'M', 'B', 'L', '2'
};

struct rustos_dvm_block_uio {
	struct uio_info uio;
	void __iomem *doorbell;
	resource_size_t shared_start;
	resource_size_t shared_bytes;
};

static bool rustos_dvm_block_size_valid(u32 bytes)
{
	return bytes >= 512U && is_power_of_2(bytes) && !(bytes % 512U);
}

static bool rustos_dvm_cursor_pair_valid(u64 producer, u64 consumer)
{
	return producer >= consumer &&
	       producer - consumer <= RUSTOS_DVM_BLOCK_QUEUE_DEPTH;
}

static int rustos_dvm_block_validate_aperture(struct pci_dev *pdev)
{
	struct resource *shared =
		&pdev->resource[RUSTOS_IVSHMEM_SHARED_BAR];
	void __iomem *mapped;
	u8 bytes[RUSTOS_DVM_BLOCK_RECORD_BYTES];
	u64 features;
	u64 capacity;
	u64 request_producer;
	u64 request_consumer;
	u64 completion_producer;
	u64 completion_consumer;
	u32 logical;
	u32 physical;
	u32 flags;
	int result = -ENODEV;

	if (!(shared->flags & IORESOURCE_MEM) ||
	    !(shared->flags & IORESOURCE_PREFETCH) ||
	    resource_size(shared) != RUSTOS_DVM_BLOCK_APERTURE_BYTES)
		return -ENODEV;
	/*
	 * BAR2 is shared RAM and every live participant maps it WB.  Do not use
	 * pci_iomap() even for header discovery: on x86 that may create a
	 * temporary UC alias while RustOS and the relay retain WB mappings.
	 */
	mapped = ioremap_cache(shared->start, sizeof(bytes));
	if (!mapped)
		return -ENOMEM;
	memcpy_fromio(bytes, mapped, sizeof(bytes));

	features = get_unaligned_le64(bytes + 32U);
	capacity = get_unaligned_le64(bytes + 48U);
	logical = get_unaligned_le32(bytes + 56U);
	physical = get_unaligned_le32(bytes + 60U);
	flags = get_unaligned_le32(bytes + 64U);
	request_producer = get_unaligned_le64(bytes + 72U);
	request_consumer = get_unaligned_le64(bytes + 80U);
	completion_producer = get_unaligned_le64(bytes + 88U);
	completion_consumer = get_unaligned_le64(bytes + 96U);

	if (!memcmp(bytes, rustos_dvm_block_magic,
		    sizeof(rustos_dvm_block_magic)) &&
	    get_unaligned_le32(bytes + 8U) == RUSTOS_DVM_BLOCK_VERSION &&
	    get_unaligned_le32(bytes + 12U) == RUSTOS_DVM_BLOCK_HEADER_BYTES &&
	    get_unaligned_le64(bytes + 16U) ==
		    RUSTOS_DVM_BLOCK_APERTURE_BYTES &&
	    get_unaligned_le32(bytes + 24U) ==
		    RUSTOS_DVM_BLOCK_QUEUE_DEPTH &&
	    get_unaligned_le32(bytes + 28U) == RUSTOS_DVM_BLOCK_SLOT_BYTES &&
	    !(features & ~RUSTOS_DVM_BLOCK_KNOWN_FEATURES) &&
	    (features & RUSTOS_DVM_BLOCK_REQUIRED_FEATURES) ==
		    RUSTOS_DVM_BLOCK_REQUIRED_FEATURES &&
	    get_unaligned_le64(bytes + 40U) != 0 && capacity != 0 &&
	    rustos_dvm_block_size_valid(logical) &&
	    rustos_dvm_block_size_valid(physical) && physical >= logical &&
	    !(physical % logical) && !(flags & ~RUSTOS_DVM_BLOCK_KNOWN_FLAGS) &&
	    (!(flags & BIT(1)) ||
	     (flags & RUSTOS_DVM_BLOCK_FLAG_RUSTOS_READY)) &&
	    !memchr_inv(bytes + 68U, 0, 4U) &&
	    memchr_inv(bytes + 104U, 0, 64U) &&
	    !memchr_inv(bytes + 168U, 0, 24U) &&
	    rustos_dvm_cursor_pair_valid(request_producer,
					 request_consumer) &&
	    rustos_dvm_cursor_pair_valid(completion_producer,
					 completion_consumer))
		result = 0;

	iounmap(mapped);
	return result;
}

static irqreturn_t rustos_dvm_block_irq(int irq, struct uio_info *info)
{
	return IRQ_HANDLED;
}

static int rustos_dvm_block_irq_control(struct uio_info *info, s32 irq_on)
{
	struct rustos_dvm_block_uio *state = info->priv;

	if (!state || irq_on != 1)
		return -EINVAL;
	/*
	 * Relay data and its Release completion cursor must be globally visible
	 * before the fixed peer-0/vector-0 doorbell. The encoded value is zero.
	 */
	wmb();
	writel(0, state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
	return 0;
}

static int rustos_dvm_block_mmap(struct uio_info *info,
				 struct vm_area_struct *vma)
{
	struct rustos_dvm_block_uio *state = info->priv;
	unsigned long mapped_bytes = vma->vm_end - vma->vm_start;

	if (!state || vma->vm_pgoff != 0 || !mapped_bytes ||
	    mapped_bytes > state->shared_bytes)
		return -EINVAL;
	vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
	/*
	 * BAR2 is QEMU shared RAM, not controller MMIO. RustOS maps the same
	 * aperture cacheable; keep x86 PAT cache bits clear for coherent WB
	 * atomics and bulk 64-KiB transfer slots.
	 */
	vma->vm_page_prot = __pgprot(pgprot_val(vma->vm_page_prot) &
				     ~_PAGE_CACHE_MASK);
	return remap_pfn_range(vma, vma->vm_start,
			       state->shared_start >> PAGE_SHIFT, mapped_bytes,
			       vma->vm_page_prot);
}

static void rustos_dvm_block_free_vectors(void *data)
{
	pci_free_irq_vectors(data);
}

static int rustos_dvm_block_probe(struct pci_dev *pdev,
				  const struct pci_device_id *id)
{
	struct rustos_dvm_block_uio *state;
	struct resource *registers;
	struct resource *shared;
	int result;

	result = pcim_enable_device(pdev);
	if (result)
		return result;
	result = rustos_dvm_block_validate_aperture(pdev);
	if (result)
		return result;
	if (pci_msix_vec_count(pdev) != RUSTOS_DVM_BLOCK_MSIX_VECTORS)
		return -ENODEV;
	result = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_MSIX);
	if (result < 0)
		return result;
	result = devm_add_action_or_reset(&pdev->dev,
					 rustos_dvm_block_free_vectors, pdev);
	if (result)
		return result;

	registers = &pdev->resource[RUSTOS_IVSHMEM_REGISTERS_BAR];
	shared = &pdev->resource[RUSTOS_IVSHMEM_SHARED_BAR];
	if (!(registers->flags & IORESOURCE_MEM) ||
	    resource_size(registers) <
		    RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32) ||
	    !(shared->flags & IORESOURCE_MEM) ||
	    !(shared->flags & IORESOURCE_PREFETCH) ||
	    resource_size(shared) != RUSTOS_DVM_BLOCK_APERTURE_BYTES)
		return -ENODEV;

	state = devm_kzalloc(&pdev->dev, sizeof(*state), GFP_KERNEL);
	if (!state)
		return -ENOMEM;
	state->doorbell = pcim_iomap(
		pdev, RUSTOS_IVSHMEM_REGISTERS_BAR,
		RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32));
	if (!state->doorbell)
		return -ENOMEM;
	state->shared_start = shared->start;
	state->shared_bytes = resource_size(shared);

	state->uio.name = RUSTOS_DVM_BLOCK_UIO_NAME;
	state->uio.version = "1";
	state->uio.irq = pci_irq_vector(pdev, 0);
	state->uio.handler = rustos_dvm_block_irq;
	state->uio.irqcontrol = rustos_dvm_block_irq_control;
	state->uio.mmap = rustos_dvm_block_mmap;
	state->uio.mem[0].name = "rustos-dvm-block-aperture-wb";
	state->uio.mem[0].memtype = UIO_MEM_PHYS;
	state->uio.mem[0].addr = shared->start;
	state->uio.mem[0].size = resource_size(shared);
	state->uio.priv = state;
	pci_set_drvdata(pdev, state);

	result = devm_uio_register_device(&pdev->dev, &state->uio);
	if (!result)
		dev_info(&pdev->dev,
			 "RustOS block UIO bound: MSI-X vector=%ld BAR2=%pa+%pa\n",
			 (long)state->uio.irq, &shared->start,
			 &state->shared_bytes);
	return result;
}

static const struct pci_device_id rustos_dvm_block_ids[] = {
	{ PCI_DEVICE(RUSTOS_IVSHMEM_VENDOR_ID, RUSTOS_IVSHMEM_DEVICE_ID) },
	{ }
};
MODULE_DEVICE_TABLE(pci, rustos_dvm_block_ids);

static struct pci_driver rustos_dvm_block_driver = {
	.name = "rustos-dvm-block-uio",
	.id_table = rustos_dvm_block_ids,
	.probe = rustos_dvm_block_probe,
};
module_pci_driver(rustos_dvm_block_driver);

MODULE_AUTHOR("RustOS");
MODULE_DESCRIPTION("RustOS fixed storage-DVM ivshmem MSI-X adapter");
MODULE_LICENSE("GPL");
MODULE_VERSION("1");
