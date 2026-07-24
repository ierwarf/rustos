// SPDX-License-Identifier: GPL-2.0-only
/*
 * RustOS GUI-DVM three-surface MSI-X -> UIO adapter.
 *
 * BAR2 is an uncached control plane and is never mapped to userspace. A
 * separate cacheable pixel region is exposed WB and read-only; VM_WRITE and
 * VM_MAYWRITE are rejected. Userspace cannot write shared control state or
 * select a doorbell peer/vector. Each immutable slot can be exported as a
 * read-only DMA-BUF for direct KMS scanout. A completed page-flip fence is
 * returned through one fixed 64-byte RELEASE record which this module validates
 * against the matching host PRESENT record before it writes the return slot and
 * rings the sole host control vector.
 */

#include <linux/io.h>
#include <linux/dma-buf.h>
#include <linux/dma-fence.h>
#include <linux/dma-map-ops.h>
#include <linux/dma-mapping.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/memremap.h>
#include <linux/mm.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/overflow.h>
#include <linux/scatterlist.h>
#include <linux/string.h>
#include <linux/sync_file.h>
#include <linux/uaccess.h>
#include <linux/uio_driver.h>
#include <asm/pgtable.h>

#define RUSTOS_IVSHMEM_VENDOR_ID 0x1af4
#define RUSTOS_IVSHMEM_DEVICE_ID 0x1110
#define RUSTOS_IVSHMEM_REGISTERS_BAR 0
#define RUSTOS_IVSHMEM_SHARED_BAR 2
#define RUSTOS_IVSHMEM_DOORBELL_OFFSET 12
#define RUSTOS_DVM_HOST_CONTROL_VECTOR 0
#define RUSTOS_DVM_HOST_OFFLINE_VECTOR 1
#define RUSTOS_DVM_MSIX_VECTOR_COUNT 2
#define RUSTOS_GUI_PIXEL_REGION_PHYS 0x100000000ULL
#define RUSTOS_GUI_PIXEL_REGION_BYTES (128ULL * 1024ULL * 1024ULL)

#define RUSTOS_GUI_POOL_HEADER_BYTES 4096
#define RUSTOS_GUI_POOL_VERSION 2
#define RUSTOS_GUI_POOL_SLOT_COUNT 3
#define RUSTOS_GUI_POOL_RECORD_BYTES 64
#define RUSTOS_GUI_POOL_HOST_RECORD_OFFSET 64
#define RUSTOS_GUI_POOL_DVM_RECORD_OFFSET 256
#define RUSTOS_GUI_POOL_DVM_SEQUENCE_OFFSET 320
#define RUSTOS_GUI_POOL_HOST_ACK_OFFSET 328
#define RUSTOS_GUI_POOL_INVITATION_OFFSET 336
#define RUSTOS_GUI_POOL_READY_ACK_OFFSET 344
#define RUSTOS_GUI_POOL_READY_CONFIRMATION_OFFSET 352

#define RUSTOS_GPU_ATLAS_HEADER_OFFSET 512
#define RUSTOS_GPU_ATLAS_HEADER_BYTES 64
#define RUSTOS_GPU_ATLAS_VERSION 3
#define RUSTOS_GPU_ATLAS_SLOT_COUNT 3
#define RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES (36ULL * 1024ULL)
#define RUSTOS_GPU_ATLAS_DAMAGE_BYTES 16
#define RUSTOS_GPU_ATLAS_MAX_DAMAGE_RECTS 64
#define RUSTOS_GPU_ATLAS_COMPLETION_BYTES 256
#define RUSTOS_GPU_ATLAS_COMPLETION_POOL_OFFSET 1024
#define RUSTOS_GPU_ATLAS_PRIME_COMPLETION_OFFSET 1792
#define RUSTOS_GPU_ATLAS_INVITATION_OFFSET 2048
#define RUSTOS_GPU_ATLAS_COMPLETION_SEQUENCE_OFFSET 2080
#define RUSTOS_GPU_ATLAS_COMPLETION_ACK_OFFSET 2112
#define RUSTOS_GPU_ATLAS_CONTEXT_ID_OFFSET 2144
#define RUSTOS_GPU_ATLAS_CONTEXT_EPOCH_OFFSET 2148
#define RUSTOS_GPU_ATLAS_PRIME_FENCE_OFFSET 2152
#define RUSTOS_GPU_PRIME_COMPLETION_BYTES 64
#define RUSTOS_GPU_RENDER_VERSION 1
#define RUSTOS_GPU_PRIME_COMPLETION_VERSION 2
#define RUSTOS_GPU_RENDER_HEADER_BYTES 64
#define RUSTOS_GPU_RENDER_SOURCE_BYTES 64
#define RUSTOS_GPU_RENDER_COMMAND_BYTES 64
#define RUSTOS_GPU_RENDER_MAX_COMMANDS 512
#define RUSTOS_GPU_RENDER_MAX_BUDGET_US 50000
#define RUSTOS_GPU_PRIME_READY 1
#define RUSTOS_GPU_PIPELINE_PRIME_MAX_NS (500ULL * 1000ULL * 1000ULL)
#define RUSTOS_GPU_ATLAS_FLAG_DVM_READ_ONLY 1
#define RUSTOS_GPU_ATLAS_EXPORT_FLAG 1
#define RUSTOS_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF 2
#define RUSTOS_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY 1

#define RUSTOS_GUI_MESSAGE_VERSION 1
#define RUSTOS_GUI_MESSAGE_KIND_PRESENT 1
#define RUSTOS_GUI_MESSAGE_KIND_RELEASE 2
#define RUSTOS_GUI_DAMAGE_FULL 1
#define RUSTOS_DVM_UIO_NAME "rustos-dvm-ivshmem-uio"
#define RUSTOS_DVM_DMABUF_NAME "rustos-dvm-display-dmabuf"
#define RUSTOS_DVM_DMABUF_IOCTL_EXPORT _IOW('R', 0x41, struct rustos_dvm_dmabuf_request)
#define RUSTOS_DVM_DMABUF_IOCTL_ACQUIRE _IOW('R', 0x42, struct rustos_dvm_acquire_request)

struct rustos_dvm_dmabuf_request {
	__u32 slot;
	__u32 flags;
};

struct rustos_dvm_acquire_request {
	__u32 slot;
	__u32 reserved;
	__u64 generation;
	__u64 sequence;
	__u64 acquire_value;
};

struct rustos_dvm_dmabuf_export {
	phys_addr_t phys;
	size_t bytes;
	struct dev_pagemap *pgmap;
};

struct rustos_dvm_ivshmem {
	struct uio_info uio;
	struct miscdevice dmabuf_misc;
	void __iomem *doorbell;
	void __iomem *shared;
	struct dev_pagemap pixel_pgmap;
	void *pixels;
	u64 slot_bytes;
	u64 gpu_command_offset;
	u64 gpu_atlas_offset;
	u64 gpu_atlas_slot_bytes;
	u32 gpu_atlas_width;
	u32 gpu_atlas_height;
	u32 gpu_atlas_stride;
	atomic_t host_invited;
	atomic_t relay_ready;
	atomic_t gpu_prime_ready;
	atomic64_t invitation_generation;
	atomic64_t control_sequence;
	u64 acquired_values[RUSTOS_GPU_ATLAS_SLOT_COUNT];
	struct mutex control_lock;
};

static const u8 rustos_gui_pool_magic[] = "RSGUI002";
static const u8 rustos_gui_message_magic[] = "RSGUI001";
static const u8 rustos_gpu_atlas_magic[] = "RSGPUA01";
static const u8 rustos_gpu_submit_magic[] = "RSGPUQ01";
static const u8 rustos_gpu_completion_magic[] = "RSGPUC01";
static const u8 rustos_gpu_render_completion_magic[] = "RSGPUD01";
static const u8 rustos_gpu_present_completion_magic[] = "RSGPUF01";
static const u8 rustos_gpu_prime_completion_magic[] = "RSGPUP01";

static int rustos_dvm_gpu_acquire_sync_fd(struct rustos_dvm_ivshmem *state,
					   unsigned long argument);

static bool rustos_dvm_pgmap_owns_pfn(struct dev_pagemap *pgmap,
				      unsigned long pfn)
{
	struct page *page;

	if (!pgmap || !pfn_valid(pfn))
		return false;
	page = pfn_to_page(pfn);
	return is_zone_device_page(page) && page->pgmap == pgmap;
}

static struct sg_table *rustos_dvm_dmabuf_map(struct dma_buf_attachment *attachment,
					      enum dma_data_direction direction)
{
	struct rustos_dvm_dmabuf_export *export = attachment->dmabuf->priv;
	struct sg_table *table;
	struct scatterlist *entry;
	unsigned int index;
	unsigned int pages;

	/* DRM PRIME asks exporters for DMA_BIDIRECTIONAL even for scanout-only
	 * imports. Accept that API request, but deliberately map the pages with
	 * DMA_TO_DEVICE permissions so the IOMMU never grants device writes to
	 * RustOS-owned pixels. Explicit device-to-memory imports stay forbidden. */
	if (direction != DMA_TO_DEVICE && direction != DMA_BIDIRECTIONAL)
		return ERR_PTR(-EPERM);
	/* The producer lives in a different VM, so this exporter cannot perform a
	 * meaningful guest-side cache clean on its behalf. The enabled x86 AMD
	 * topology is coherent; reject any future non-coherent attachment instead
	 * of silently relying on DMA_ATTR_SKIP_CPU_SYNC there. */
	if (!dev_is_dma_coherent(attachment->dev))
		return ERR_PTR(-EOPNOTSUPP);
	if (!export || !export->pgmap || !export->bytes ||
	    export->phys & ~PAGE_MASK || export->bytes & ~PAGE_MASK)
		return ERR_PTR(-EINVAL);
	pages = export->bytes >> PAGE_SHIFT;
	table = kzalloc(sizeof(*table), GFP_KERNEL);
	if (!table)
		return ERR_PTR(-ENOMEM);
	if (sg_alloc_table(table, pages, GFP_KERNEL)) {
		kfree(table);
		return ERR_PTR(-ENOMEM);
	}
	for_each_sg(table->sgl, entry, pages, index) {
		unsigned long pfn = PHYS_PFN(export->phys) + index;

		/*
		 * pgmap_pfn_valid() is an in-kernel helper, not an exported module
		 * symbol. The page's zone and pgmap identity are the module-safe
		 * ownership check and reject PFNs outside this dev_pagemap.
		 */
		if (!rustos_dvm_pgmap_owns_pfn(export->pgmap, pfn)) {
			pr_err_ratelimited("rustos-dvm-display: DMA-BUF page-map ownership lost pfn=%#lx\n",
					   pfn);
			sg_free_table(table);
			kfree(table);
			return ERR_PTR(-ENXIO);
		}
		sg_set_page(entry, pfn_to_page(pfn), PAGE_SIZE, 0);
	}
	if (dma_map_sgtable(attachment->dev, table, DMA_TO_DEVICE,
			    DMA_ATTR_SKIP_CPU_SYNC)) {
		dev_err_ratelimited(attachment->dev,
				    "rustos-dvm-display: GPU DMA-BUF map failed phys=%pa bytes=%zu\n",
				    &export->phys, export->bytes);
		sg_free_table(table);
		kfree(table);
		return ERR_PTR(-EIO);
	}
	return table;
}

static void rustos_dvm_dmabuf_unmap(struct dma_buf_attachment *attachment,
				    struct sg_table *table,
				    enum dma_data_direction direction)
{
	(void)direction;
	if (!table)
		return;
	dma_unmap_sgtable(attachment->dev, table, DMA_TO_DEVICE,
			  DMA_ATTR_SKIP_CPU_SYNC);
	sg_free_table(table);
	kfree(table);
}

static int rustos_dvm_dmabuf_mmap(struct dma_buf *dmabuf, struct vm_area_struct *vma)
{
	return -EPERM;
}

static void rustos_dvm_dmabuf_release(struct dma_buf *dmabuf)
{
	struct rustos_dvm_dmabuf_export *export = dmabuf->priv;

	if (export) {
		put_dev_pagemap(export->pgmap);
		memzero_explicit(export, sizeof(*export));
		kfree(export);
	}
}

static const struct dma_buf_ops rustos_dvm_dmabuf_ops = {
	.map_dma_buf = rustos_dvm_dmabuf_map,
	.unmap_dma_buf = rustos_dvm_dmabuf_unmap,
	.mmap = rustos_dvm_dmabuf_mmap,
	.release = rustos_dvm_dmabuf_release,
};

static int rustos_dvm_dmabuf_open(struct inode *inode, struct file *file)
{
	struct miscdevice *misc = file->private_data;
	struct rustos_dvm_ivshmem *state;

	state = container_of(misc, struct rustos_dvm_ivshmem, dmabuf_misc);
	if (!state)
		return -ENODEV;
	/*
	 * DMA-BUF import and the local bootstrap modeset precede the relay-ready
	 * acknowledgement. Requiring an invitation or relay_ready here creates a
	 * lifecycle cycle in which readiness can never be reached. The module has
	 * already validated the fixed pool and exports only device-read mappings;
	 * opening it early grants no device-write or host-control authority.
	 */
	file->private_data = state;
	return nonseekable_open(inode, file);
}

static long rustos_dvm_dmabuf_ioctl(struct file *file, unsigned int command,
				    unsigned long argument)
{
	struct rustos_dvm_ivshmem *state = file->private_data;
	struct rustos_dvm_dmabuf_request request;
	struct rustos_dvm_dmabuf_export *export;
	DEFINE_DMA_BUF_EXPORT_INFO(info);
	struct dma_buf *dmabuf;
	phys_addr_t end_phys;
	int fd;

	if (command == RUSTOS_DVM_DMABUF_IOCTL_ACQUIRE)
		return rustos_dvm_gpu_acquire_sync_fd(state, argument);
	if (command != RUSTOS_DVM_DMABUF_IOCTL_EXPORT ||
	    copy_from_user(&request, (void __user *)argument, sizeof(request)))
		return -EINVAL;
	if (!state || request.flags & ~RUSTOS_GPU_ATLAS_EXPORT_FLAG ||
    request.slot >= RUSTOS_GUI_POOL_SLOT_COUNT || !state->slot_bytes)
		return -EPERM;
	export = kzalloc(sizeof(*export), GFP_KERNEL);
	if (!export)
		return -ENOMEM;
	if (request.flags == RUSTOS_GPU_ATLAS_EXPORT_FLAG) {
		if (!state->gpu_atlas_offset || !state->gpu_atlas_slot_bytes) {
			kfree(export);
			return -ENODEV;
		}
		export->phys = RUSTOS_GUI_PIXEL_REGION_PHYS + state->gpu_atlas_offset +
			       request.slot * state->gpu_atlas_slot_bytes;
		export->bytes = state->gpu_atlas_slot_bytes;
	} else {
		export->phys = RUSTOS_GUI_PIXEL_REGION_PHYS + RUSTOS_GUI_POOL_HEADER_BYTES +
			       request.slot * state->slot_bytes;
		export->bytes = state->slot_bytes;
	}
	if (check_add_overflow(export->phys, export->bytes - 1U, &end_phys)) {
		kfree(export);
		return -EOVERFLOW;
	}
	export->pgmap = get_dev_pagemap(PHYS_PFN(export->phys), NULL);
	if (!rustos_dvm_pgmap_owns_pfn(export->pgmap, PHYS_PFN(end_phys))) {
		put_dev_pagemap(export->pgmap);
		kfree(export);
		return -ENXIO;
	}
	info.ops = &rustos_dvm_dmabuf_ops;
	info.size = export->bytes;
	info.flags = O_RDONLY;
	info.priv = export;
	dmabuf = dma_buf_export(&info);
	if (IS_ERR(dmabuf)) {
		fd = PTR_ERR(dmabuf);
		put_dev_pagemap(export->pgmap);
		kfree(export);
		return fd;
	}
	fd = dma_buf_fd(dmabuf, O_CLOEXEC);
	if (fd < 0)
		dma_buf_put(dmabuf);
	return fd;
}

static const struct file_operations rustos_dvm_dmabuf_fops = {
	.owner = THIS_MODULE,
	.open = rustos_dvm_dmabuf_open,
	.unlocked_ioctl = rustos_dvm_dmabuf_ioctl,
#ifdef CONFIG_COMPAT
	.compat_ioctl = rustos_dvm_dmabuf_ioctl,
#endif
};

static u32 rustos_dvm_read_le32(const u8 *bytes)
{
	return (u32)bytes[0] | ((u32)bytes[1] << 8) |
		((u32)bytes[2] << 16) | ((u32)bytes[3] << 24);
}

static u64 rustos_dvm_read_le64(const u8 *bytes)
{
	u64 value = 0;
	unsigned int index;

	for (index = 0; index < sizeof(value); index++)
		value |= (u64)bytes[index] << (index * 8);
	return value;
}

static bool rustos_dvm_copy_pixels(struct rustos_dvm_ivshmem *state,
				   u64 physical, void *destination,
				   size_t bytes)
{
	u64 offset;
	u64 end;

	if (!state || !state->pixels || !destination || !bytes ||
	    physical < RUSTOS_GUI_PIXEL_REGION_PHYS)
		return false;
	offset = physical - RUSTOS_GUI_PIXEL_REGION_PHYS;
	if (check_add_overflow(offset, (u64)bytes, &end) ||
	    end > RUSTOS_GUI_PIXEL_REGION_BYTES)
		return false;
	memcpy(destination, (u8 *)state->pixels + offset, bytes);
	return true;
}

static bool rustos_dvm_valid_gpu_acquire(
	struct rustos_dvm_ivshmem *state,
	const struct rustos_dvm_acquire_request *request)
{
	u8 command[64];
	u8 batch[128];
	u32 batch_bytes;
	u32 command_count;
	u32 damage_count;
	u64 command_phys;
	u64 damage_bytes;
	u64 batch_phys;
	u64 batch_end;
	u64 expected_batch_bytes;
	u64 slot_end;

	if (!state || !request || request->reserved ||
	    !atomic_read(&state->relay_ready) ||
	    request->slot >= RUSTOS_GPU_ATLAS_SLOT_COUNT ||
	    !request->generation || !request->sequence || !request->acquire_value)
		return false;

	/* The host publishes pixels and the exact command record before ringing
	 * MSI-X. The UIO read precedes this ioctl; complete the device-to-CPU
	 * acquire ordering before validating the shared generation and issuing a
	 * sync_file that the GPU command stream will wait on. */
	dma_rmb();
	if (readq(state->shared + RUSTOS_GPU_ATLAS_INVITATION_OFFSET +
		  request->slot * sizeof(u64)) != request->sequence ||
	    readq(state->shared + RUSTOS_GPU_ATLAS_COMPLETION_ACK_OFFSET +
		  request->slot * sizeof(u64)) == request->sequence)
		return false;

	if (check_mul_overflow((u64)request->slot,
			       (u64)RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES,
			       &command_phys) ||
	    check_add_overflow(command_phys, state->gpu_command_offset,
			       &command_phys) ||
	    check_add_overflow(command_phys, RUSTOS_GUI_PIXEL_REGION_PHYS,
			       &command_phys))
		return false;
	if (!rustos_dvm_copy_pixels(state, command_phys, command,
				    sizeof(command)))
		return false;

	batch_bytes = rustos_dvm_read_le32(command + 20);
	damage_count = rustos_dvm_read_le32(command + 56);
	if (memcmp(command, rustos_gpu_submit_magic, 8) ||
	    rustos_dvm_read_le32(command + 8) != RUSTOS_GPU_ATLAS_VERSION ||
	    rustos_dvm_read_le32(command + 12) != sizeof(command) ||
	    rustos_dvm_read_le32(command + 16) != request->slot ||
	    rustos_dvm_read_le64(command + 24) != request->generation ||
	    rustos_dvm_read_le64(command + 32) != request->sequence ||
	    !rustos_dvm_read_le32(command + 40) ||
	    rustos_dvm_read_le32(command + 44) !=
		    RUSTOS_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF ||
	    !rustos_dvm_read_le64(command + 48) ||
	    damage_count > RUSTOS_GPU_ATLAS_MAX_DAMAGE_RECTS ||
	    memchr_inv(command + 60, 0, 4) || batch_bytes < sizeof(batch) ||
	    check_mul_overflow((u64)damage_count,
			       (u64)RUSTOS_GPU_ATLAS_DAMAGE_BYTES,
			       &damage_bytes) ||
	    check_add_overflow(command_phys, (u64)sizeof(command), &batch_phys) ||
	    check_add_overflow(batch_phys, damage_bytes, &batch_phys) ||
	    check_add_overflow(batch_phys, (u64)batch_bytes, &batch_end) ||
	    check_add_overflow(command_phys,
			       (u64)RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES,
			       &slot_end) || batch_end > slot_end)
		return false;

	if (!rustos_dvm_copy_pixels(state, batch_phys, batch, sizeof(batch)))
		return false;
	command_count = rustos_dvm_read_le32(batch + 20);
	if (!command_count || command_count > RUSTOS_GPU_RENDER_MAX_COMMANDS ||
	    check_mul_overflow((u64)command_count,
			       (u64)RUSTOS_GPU_RENDER_COMMAND_BYTES,
			       &expected_batch_bytes) ||
	    check_add_overflow(expected_batch_bytes,
			       (u64)RUSTOS_GPU_RENDER_HEADER_BYTES +
			       RUSTOS_GPU_RENDER_SOURCE_BYTES,
			       &expected_batch_bytes) ||
	    expected_batch_bytes != batch_bytes)
		return false;
	return !memcmp(batch, "RSGPU001", 8) &&
	       rustos_dvm_read_le32(batch + 8) == 1 &&
	       rustos_dvm_read_le32(batch + 12) == RUSTOS_GPU_RENDER_HEADER_BYTES &&
	       rustos_dvm_read_le32(batch + 16) == RUSTOS_GPU_RENDER_COMMAND_BYTES &&
	       rustos_dvm_read_le32(batch + 24) != 0 &&
	       rustos_dvm_read_le32(batch + 28) ==
		       rustos_dvm_read_le32(command + 40) &&
	       rustos_dvm_read_le64(batch + 32) != 0 &&
	       rustos_dvm_read_le64(batch + 40) == request->acquire_value &&
	       request->acquire_value <= rustos_dvm_read_le64(batch + 32) &&
	       rustos_dvm_read_le32(batch + 48) != 0 &&
	       rustos_dvm_read_le32(batch + 48) <= RUSTOS_GPU_RENDER_MAX_BUDGET_US &&
	       rustos_dvm_read_le32(batch + 52) == 1 &&
	       rustos_dvm_read_le32(batch + 56) == 1 &&
	       !memchr_inv(batch + 60, 0, 4) &&
	       rustos_dvm_read_le64(batch + 64) != 0 &&
	       rustos_dvm_read_le64(batch + 72) == request->generation &&
	       rustos_dvm_read_le64(batch + 80) == request->acquire_value &&
	       rustos_dvm_read_le32(batch + 88) == state->gpu_atlas_width &&
	       rustos_dvm_read_le32(batch + 92) == state->gpu_atlas_height &&
	       rustos_dvm_read_le32(batch + 96) == state->gpu_atlas_stride &&
	       rustos_dvm_read_le32(batch + 100) == 1 &&
	       rustos_dvm_read_le32(batch + 104) == 3 &&
	       rustos_dvm_read_le32(batch + 108) == request->slot &&
	       rustos_dvm_read_le64(batch + 112) ==
		       rustos_dvm_read_le64(command + 48) &&
	       !memchr_inv(batch + 120, 0, 8);
}

static int rustos_dvm_gpu_acquire_sync_fd(struct rustos_dvm_ivshmem *state,
					   unsigned long argument)
{
	struct rustos_dvm_acquire_request request;
	struct dma_fence *fence;
	struct sync_file *sync_file;
	int fd;
	int result;

	if (copy_from_user(&request, (void __user *)argument, sizeof(request)))
		return -EFAULT;
	if (!state)
		return -ENODEV;
	fd = get_unused_fd_flags(O_CLOEXEC);
	if (fd < 0)
		return fd;
	result = 0;
	mutex_lock(&state->control_lock);
	if (request.slot >= RUSTOS_GPU_ATLAS_SLOT_COUNT ||
	    request.acquire_value <= state->acquired_values[request.slot] ||
	    !rustos_dvm_valid_gpu_acquire(state, &request)) {
		result = -EPERM;
		goto out_unlock;
	}
	fence = dma_fence_get_stub();
	if (!fence) {
		result = -ENOMEM;
		goto out_unlock;
	}
	sync_file = sync_file_create(fence);
	dma_fence_put(fence);
	if (!sync_file) {
		result = -ENOMEM;
		goto out_unlock;
	}
	state->acquired_values[request.slot] = request.acquire_value;
	mutex_unlock(&state->control_lock);
	fd_install(fd, sync_file->file);
	return fd;

out_unlock:
	mutex_unlock(&state->control_lock);
	put_unused_fd(fd);
	return result;
}

static bool rustos_dvm_valid_gpu_atlas_header(const u8 *bytes)
{
	u64 region_bytes;
	u64 command_offset;
	u64 command_bytes;
	u64 command_end;
	u64 atlas_offset;
	u64 atlas_slot_bytes;
	u64 atlas_bytes;
	u64 atlas_end;
	u32 width;
	u32 height;
	u32 stride;

	if (memcmp(bytes, rustos_gpu_atlas_magic,
		   sizeof(rustos_gpu_atlas_magic) - 1) ||
	    rustos_dvm_read_le32(bytes + 8) != RUSTOS_GPU_ATLAS_VERSION ||
	    rustos_dvm_read_le32(bytes + 12) != RUSTOS_GPU_ATLAS_HEADER_BYTES ||
	    rustos_dvm_read_le32(bytes + 60) !=
		    RUSTOS_GPU_ATLAS_FLAG_DVM_READ_ONLY)
		return false;
	region_bytes = rustos_dvm_read_le64(bytes + 16);
	command_offset = rustos_dvm_read_le64(bytes + 24);
	atlas_offset = rustos_dvm_read_le64(bytes + 32);
	atlas_slot_bytes = rustos_dvm_read_le64(bytes + 40);
	width = rustos_dvm_read_le32(bytes + 48);
	height = rustos_dvm_read_le32(bytes + 52);
	stride = rustos_dvm_read_le32(bytes + 56);
	if (region_bytes != RUSTOS_GUI_PIXEL_REGION_BYTES ||
	    !command_offset || command_offset & (PAGE_SIZE - 1) ||
	    !atlas_offset || atlas_offset & (PAGE_SIZE - 1) ||
	    !width || !height || width > 8192 || height > 8192 ||
	    (u64)stride < (u64)width * 4 || stride & 3 ||
	    atlas_slot_bytes != (u64)stride * height ||
	    !atlas_slot_bytes || atlas_slot_bytes & (PAGE_SIZE - 1))
		return false;
	if (check_mul_overflow((u64)RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES,
			       (u64)RUSTOS_GPU_ATLAS_SLOT_COUNT,
			       &command_bytes) ||
	    check_add_overflow(command_offset, command_bytes, &command_end) ||
	    check_mul_overflow(atlas_slot_bytes,
			       (u64)RUSTOS_GPU_ATLAS_SLOT_COUNT, &atlas_bytes) ||
	    check_add_overflow(atlas_offset, atlas_bytes, &atlas_end))
		return false;
	return command_end <= atlas_offset && atlas_end <= region_bytes;
}

static bool rustos_dvm_host_present_matches(struct rustos_dvm_ivshmem *state,
						      const u8 *record)
{
	u32 slot = rustos_dvm_read_le32(record + 16);
	u64 generation = rustos_dvm_read_le64(record + 24);
	u8 present[RUSTOS_GUI_POOL_RECORD_BYTES];

	if (slot >= RUSTOS_GUI_POOL_SLOT_COUNT)
		return false;
	memcpy_fromio(present, state->shared + RUSTOS_GUI_POOL_HOST_RECORD_OFFSET +
		      slot * RUSTOS_GUI_POOL_RECORD_BYTES, sizeof(present));
	return !memcmp(present, rustos_gui_message_magic, sizeof(rustos_gui_message_magic) - 1) &&
		rustos_dvm_read_le32(present + 8) == RUSTOS_GUI_MESSAGE_VERSION &&
		rustos_dvm_read_le32(present + 12) == RUSTOS_GUI_MESSAGE_KIND_PRESENT &&
		rustos_dvm_read_le32(present + 16) == slot &&
		rustos_dvm_read_le64(present + 24) == generation;
}

static bool rustos_dvm_valid_release(struct rustos_dvm_ivshmem *state,
					     const u8 *record)
{
	if (memcmp(record, rustos_gui_message_magic, sizeof(rustos_gui_message_magic) - 1) ||
	    rustos_dvm_read_le32(record + 8) != RUSTOS_GUI_MESSAGE_VERSION ||
	    rustos_dvm_read_le32(record + 12) != RUSTOS_GUI_MESSAGE_KIND_RELEASE ||
	    rustos_dvm_read_le32(record + 16) >= RUSTOS_GUI_POOL_SLOT_COUNT ||
	    memchr_inv(record + 20, 0, 4) ||
	    !rustos_dvm_read_le64(record + 24) ||
	    rustos_dvm_read_le64(record + 24) & 1 ||
	    memchr_inv(record + 32, 0, 16) ||
	    rustos_dvm_read_le32(record + 48) != RUSTOS_GUI_DAMAGE_FULL ||
	    memchr_inv(record + 52, 0, 12))
		return false;
	return rustos_dvm_host_present_matches(state, record);
}

static void rustos_dvm_ivshmem_latch_host_invitation(struct rustos_dvm_ivshmem *state)
{
	u64 generation;

	/* The host records this exact even generation before ringing peer 1. */
	generation = readq(state->shared + RUSTOS_GUI_POOL_INVITATION_OFFSET);
	if (!generation || generation & 1)
		return;
	atomic64_set(&state->invitation_generation, generation);
	atomic_set(&state->host_invited, 1);

}

static irqreturn_t rustos_dvm_ivshmem_irq(int irq, struct uio_info *info)
{
	struct rustos_dvm_ivshmem *state = info->priv;

	rustos_dvm_ivshmem_latch_host_invitation(state);
	return IRQ_HANDLED;
}

static int rustos_dvm_ivshmem_irq_control(struct uio_info *info, s32 irq_on)
{
	/* Eventfd-backed MSI-X has no guest-controlled INTx mask state. */
	return 0;
}

static int rustos_dvm_ivshmem_mmap(struct uio_info *info,
				   struct vm_area_struct *vma)
{
	struct rustos_dvm_ivshmem *state = info->priv;
	struct uio_mem *memory = &info->mem[0];
	unsigned long mapped_bytes = vma->vm_end - vma->vm_start;

	/*
	 * The bulk pixel pool is a QEMU memory device, not PCI MMIO. Generic UIO
	 * forces UIO_MEM_PHYS through pgprot_noncached(), so keep this exact range
	 * write-back and read-only. The separate ivshmem BAR remains uncached and
	 * carries only authenticated control records and doorbells.
	 */
	if (!state || vma->vm_pgoff != 0 || !mapped_bytes ||
	    mapped_bytes > memory->size)
		return -EINVAL;
	if (vma->vm_flags & (VM_WRITE | VM_MAYWRITE))
		return -EPERM;
	vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
	/* x86 PAT encodes WB as zero: remove any inherited PCI cache bits. */
	vma->vm_page_prot = __pgprot(pgprot_val(vma->vm_page_prot) &
				     ~_PAGE_CACHE_MASK);
	dev_info_once(info->uio_dev->dev.parent,
		      "RustOS GUI pixel pool mapped WB read-only bytes=%lu\n",
		      mapped_bytes);
	return remap_pfn_range(vma, vma->vm_start,
			       memory->addr >> PAGE_SHIFT, mapped_bytes,
			       vma->vm_page_prot);
}

static void rustos_dvm_ivshmem_free_vectors(void *data)
{
	pci_free_irq_vectors(data);
}

static void rustos_dvm_ivshmem_unregister_misc(void *data)
{
	misc_deregister(data);
}

static ssize_t rustos_dvm_host_invited_show(struct device *dev,
					    struct device_attribute *attr, char *buf)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);

	if (!state)
		return -ENODEV;
	return sysfs_emit(buf, "%u\n", atomic_read(&state->host_invited) ? 1 : 0);
}
static DEVICE_ATTR_RO(rustos_dvm_host_invited);

static bool rustos_dvm_valid_gpu_prime(struct rustos_dvm_ivshmem *state,
					const u8 *record)
{
	u32 context_id;
	u32 context_epoch;
	u64 fence_value;
	u64 duration_ns;

	if (!state || !record ||
	    memcmp(record, rustos_gpu_prime_completion_magic, 8) ||
	    rustos_dvm_read_le32(record + 8) != RUSTOS_GPU_PRIME_COMPLETION_VERSION ||
	    rustos_dvm_read_le32(record + 12) != RUSTOS_GPU_PRIME_COMPLETION_BYTES ||
	    rustos_dvm_read_le32(record + 24) != RUSTOS_GPU_PRIME_READY ||
	    (rustos_dvm_read_le32(record + 28) !=
		     RUSTOS_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY &&
	     rustos_dvm_read_le32(record + 28) !=
		     RUSTOS_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF) ||
	    memchr_inv(record + 48, 0, 16))
		return false;
	context_id = rustos_dvm_read_le32(record + 16);
	context_epoch = rustos_dvm_read_le32(record + 20);
	fence_value = rustos_dvm_read_le64(record + 32);
	duration_ns = rustos_dvm_read_le64(record + 40);
	return context_id != 0 && context_epoch != 0 && fence_value != 0 &&
	       duration_ns != 0 && duration_ns <= RUSTOS_GPU_PIPELINE_PRIME_MAX_NS &&
	       context_id == readl(state->shared + RUSTOS_GPU_ATLAS_CONTEXT_ID_OFFSET) &&
	       context_epoch == readl(state->shared + RUSTOS_GPU_ATLAS_CONTEXT_EPOCH_OFFSET) &&
	       fence_value == readq(state->shared + RUSTOS_GPU_ATLAS_PRIME_FENCE_OFFSET);
}

static ssize_t rustos_dvm_gpu_prime_store(struct device *dev,
					   struct device_attribute *attr,
					   const char *buf, size_t count)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);
	ssize_t result = count;

	if (!state || count != RUSTOS_GPU_PRIME_COMPLETION_BYTES ||
	    !rustos_dvm_valid_gpu_prime(state, (const u8 *)buf))
		return -EPERM;
	mutex_lock(&state->control_lock);
	if (!atomic_read(&state->host_invited) || atomic_read(&state->relay_ready) ||
	    atomic_read(&state->gpu_prime_ready)) {
		result = -EAGAIN;
		goto out;
	}
	memcpy_toio(state->shared + RUSTOS_GPU_ATLAS_PRIME_COMPLETION_OFFSET,
		    buf, count);
	wmb();
	atomic_set(&state->gpu_prime_ready, 1);
out:
	mutex_unlock(&state->control_lock);
	return result;
}
static DEVICE_ATTR_WO(rustos_dvm_gpu_prime);

static ssize_t rustos_dvm_display_ready_store(struct device *dev,
					       struct device_attribute *attr,
					       const char *buf, size_t count)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);
	u64 generation;

	if (!state || count != 6 || memcmp(buf, "ready\n", 6))
		return -EINVAL;
	if (!atomic_read(&state->gpu_prime_ready))
		return -EAGAIN;
	if (atomic_cmpxchg(&state->host_invited, 1, 0) != 1)
		return -EAGAIN;
	generation = atomic64_read(&state->invitation_generation);
	if (!generation || generation & 1)
		return -EPROTO;
	writeq(generation, state->shared + RUSTOS_GUI_POOL_READY_ACK_OFFSET);
	wmb();
	atomic_set(&state->relay_ready, 1);
	iowrite32(RUSTOS_DVM_HOST_CONTROL_VECTOR,
		  state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
	return count;
}
static DEVICE_ATTR_WO(rustos_dvm_display_ready);

static ssize_t rustos_dvm_display_control_store(struct device *dev,
					  struct device_attribute *attr,
					  const char *buf, size_t count)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);
	u64 control_sequence;
	u64 next;
	ssize_t result = count;

	if (!state || count != RUSTOS_GUI_POOL_RECORD_BYTES)
		return -EINVAL;
	if (!atomic_read(&state->relay_ready) ||
	    !rustos_dvm_valid_release(state, (const u8 *)buf))
		return -EPERM;
	mutex_lock(&state->control_lock);
	control_sequence = atomic64_read(&state->control_sequence);
	if (readq(state->shared + RUSTOS_GUI_POOL_HOST_ACK_OFFSET) != control_sequence) {
		result = -EAGAIN;
		goto out;
	}
	next = control_sequence + 1;
	if (!next)
		next = 1;
	memcpy_toio(state->shared + RUSTOS_GUI_POOL_DVM_RECORD_OFFSET, buf, count);
	wmb();
	writeq(next, state->shared + RUSTOS_GUI_POOL_DVM_SEQUENCE_OFFSET);
	wmb();
	atomic64_set(&state->control_sequence, next);
	iowrite32(RUSTOS_DVM_HOST_CONTROL_VECTOR,
		  state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
out:
	mutex_unlock(&state->control_lock);
	return result;
}
static DEVICE_ATTR_WO(rustos_dvm_display_control);

static bool rustos_dvm_valid_gpu_completion(struct rustos_dvm_ivshmem *state,
					     const u8 *record)
{
	u8 command[64];
	u8 batch_header[64];
	u32 slot;
	u32 batch_bytes;
	u32 damage_count;
	u32 context_id;
	u32 context_epoch;
	u32 output_index;
	u64 generation;
	u64 sequence;
	u64 submit_value;
	u64 invitation;
	u64 acknowledged;
	u64 command_phys;
	u64 damage_bytes;
	u64 batch_phys;
	u64 batch_end;
	u64 slot_end;

	if (!state || memcmp(record, rustos_gpu_completion_magic, 8) ||
	    rustos_dvm_read_le32(record + 8) != RUSTOS_GPU_ATLAS_VERSION ||
	    rustos_dvm_read_le32(record + 12) !=
		    RUSTOS_GPU_ATLAS_COMPLETION_BYTES ||
	    rustos_dvm_read_le32(record + 20) != 3 ||
	    memchr_inv(record + 40, 0, 24) ||
	    memchr_inv(record + 192, 0, 64))
		return false;
	slot = rustos_dvm_read_le32(record + 16);
	generation = rustos_dvm_read_le64(record + 24);
	sequence = rustos_dvm_read_le64(record + 32);
	if (slot >= RUSTOS_GPU_ATLAS_SLOT_COUNT || !generation || !sequence)
		return false;
	invitation = readq(state->shared + RUSTOS_GPU_ATLAS_INVITATION_OFFSET +
			   slot * sizeof(u64));
	acknowledged = readq(state->shared + RUSTOS_GPU_ATLAS_COMPLETION_ACK_OFFSET +
			     slot * sizeof(u64));
	if (invitation != sequence || acknowledged == sequence ||
	    readq(state->shared + RUSTOS_GPU_ATLAS_COMPLETION_SEQUENCE_OFFSET +
		  slot * sizeof(u64)) != acknowledged)
		return false;
	if (check_mul_overflow((u64)slot,
			       (u64)RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES,
			       &command_phys) ||
	    check_add_overflow(command_phys, state->gpu_command_offset,
			       &command_phys) ||
	    check_add_overflow(command_phys, RUSTOS_GUI_PIXEL_REGION_PHYS,
			       &command_phys))
		return false;
	if (!rustos_dvm_copy_pixels(state, command_phys, command,
				    sizeof(command)))
		return false;
	batch_bytes = rustos_dvm_read_le32(command + 20);
	damage_count = rustos_dvm_read_le32(command + 56);
	if (memcmp(command, rustos_gpu_submit_magic, 8) ||
	    rustos_dvm_read_le32(command + 8) != RUSTOS_GPU_ATLAS_VERSION ||
	    rustos_dvm_read_le32(command + 12) != 64 ||
	    rustos_dvm_read_le32(command + 16) != slot ||
	    rustos_dvm_read_le64(command + 24) != generation ||
	    rustos_dvm_read_le64(command + 32) != sequence ||
	    !rustos_dvm_read_le32(command + 40) ||
	    (rustos_dvm_read_le32(command + 44) != 1 &&
	     rustos_dvm_read_le32(command + 44) != 2) ||
	    !rustos_dvm_read_le64(command + 48) ||
	    damage_count > RUSTOS_GPU_ATLAS_MAX_DAMAGE_RECTS ||
	    memchr_inv(command + 60, 0, 4) || batch_bytes < 128 ||
	    check_mul_overflow((u64)damage_count,
			       (u64)RUSTOS_GPU_ATLAS_DAMAGE_BYTES,
			       &damage_bytes) ||
	    check_add_overflow(command_phys, (u64)sizeof(command), &batch_phys) ||
	    check_add_overflow(batch_phys, damage_bytes, &batch_phys) ||
	    check_add_overflow(batch_phys, (u64)batch_bytes, &batch_end) ||
	    check_add_overflow(command_phys,
			       (u64)RUSTOS_GPU_ATLAS_COMMAND_SLOT_BYTES,
			       &slot_end) || batch_end > slot_end)
		return false;
	if (!rustos_dvm_copy_pixels(state, batch_phys, batch_header,
				    sizeof(batch_header)))
		return false;
	if (memcmp(batch_header, "RSGPU001", 8))
		return false;
	context_id = rustos_dvm_read_le32(batch_header + 24);
	context_epoch = rustos_dvm_read_le32(batch_header + 28);
	submit_value = rustos_dvm_read_le64(batch_header + 32);
	if (!context_id || context_epoch != rustos_dvm_read_le32(command + 40) ||
	    !submit_value ||
	    memcmp(record + 64, rustos_gpu_render_completion_magic, 8) ||
	    memcmp(record + 128, rustos_gpu_present_completion_magic, 8) ||
	    rustos_dvm_read_le32(record + 64 + 16) != context_id ||
	    rustos_dvm_read_le32(record + 128 + 16) != context_id ||
	    rustos_dvm_read_le32(record + 64 + 20) != context_epoch ||
	    rustos_dvm_read_le32(record + 128 + 20) != context_epoch ||
	    rustos_dvm_read_le32(record + 64 + 24) != 1)
		return false;
	output_index = rustos_dvm_read_le32(record + 64 + 28);
	return output_index < RUSTOS_GPU_ATLAS_SLOT_COUNT &&
	       rustos_dvm_read_le32(record + 128 + 24) == output_index &&
	       rustos_dvm_read_le64(record + 64 + 32) == submit_value &&
	       rustos_dvm_read_le64(record + 64 + 40) == submit_value &&
	       rustos_dvm_read_le64(record + 64 + 48) != 0 &&
	       rustos_dvm_read_le64(record + 64 + 56) == submit_value &&
	       rustos_dvm_read_le64(record + 128 + 32) == submit_value &&
	       rustos_dvm_read_le64(record + 128 + 40) == submit_value &&
	       rustos_dvm_read_le64(record + 128 + 48) < submit_value &&
	       rustos_dvm_read_le64(record + 128 + 56) != 0;
}

static ssize_t rustos_dvm_gpu_completion_store(struct device *dev,
					       struct device_attribute *attr,
					       const char *buf, size_t count)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);
	u32 slot;
	u64 sequence;
	ssize_t result = count;

	if (!state || count != RUSTOS_GPU_ATLAS_COMPLETION_BYTES ||
	    !rustos_dvm_valid_gpu_completion(state, (const u8 *)buf))
		return -EPERM;
	slot = rustos_dvm_read_le32((const u8 *)buf + 16);
	sequence = rustos_dvm_read_le64((const u8 *)buf + 32);
	mutex_lock(&state->control_lock);
	if (readq(state->shared + RUSTOS_GPU_ATLAS_INVITATION_OFFSET +
		  slot * sizeof(u64)) != sequence ||
	    readq(state->shared + RUSTOS_GPU_ATLAS_COMPLETION_ACK_OFFSET +
		  slot * sizeof(u64)) == sequence) {
		result = -EAGAIN;
		goto out;
	}
	memcpy_toio(state->shared + RUSTOS_GPU_ATLAS_COMPLETION_POOL_OFFSET +
		      slot * RUSTOS_GPU_ATLAS_COMPLETION_BYTES,
		    buf, count);
	wmb();
	writeq(sequence,
	       state->shared + RUSTOS_GPU_ATLAS_COMPLETION_SEQUENCE_OFFSET +
		       slot * sizeof(u64));
	wmb();
	iowrite32(RUSTOS_DVM_HOST_CONTROL_VECTOR,
		  state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
out:
	mutex_unlock(&state->control_lock);
	return result;
}
static DEVICE_ATTR_WO(rustos_dvm_gpu_completion);

static ssize_t rustos_dvm_display_offline_store(struct device *dev,
					 struct device_attribute *attr,
					 const char *buf, size_t count)
{
	struct rustos_dvm_ivshmem *state = dev_get_drvdata(dev);

	if (!state || count != 8 || memcmp(buf, "offline\n", 8))
		return -EINVAL;
	mutex_lock(&state->control_lock);
	atomic_set(&state->host_invited, 0);
	atomic_set(&state->relay_ready, 0);
	atomic_set(&state->gpu_prime_ready, 0);
	atomic64_set(&state->invitation_generation, 0);
	memset(state->acquired_values, 0, sizeof(state->acquired_values));
	writeq(0, state->shared + RUSTOS_GUI_POOL_READY_ACK_OFFSET);
	memset_io(state->shared + RUSTOS_GPU_ATLAS_PRIME_COMPLETION_OFFSET, 0,
		  RUSTOS_GPU_PRIME_COMPLETION_BYTES);
	wmb();
	iowrite32(RUSTOS_DVM_HOST_OFFLINE_VECTOR,
		  state->doorbell + RUSTOS_IVSHMEM_DOORBELL_OFFSET);
	mutex_unlock(&state->control_lock);
	return count;
}
static DEVICE_ATTR_WO(rustos_dvm_display_offline);

static struct attribute *rustos_dvm_ivshmem_attributes[] = {
	&dev_attr_rustos_dvm_host_invited.attr,
	&dev_attr_rustos_dvm_gpu_prime.attr,
	&dev_attr_rustos_dvm_display_ready.attr,
	&dev_attr_rustos_dvm_display_control.attr,
	&dev_attr_rustos_dvm_gpu_completion.attr,
	&dev_attr_rustos_dvm_display_offline.attr,
	NULL,
};

static const struct attribute_group rustos_dvm_ivshmem_attribute_group = {
	.attrs = rustos_dvm_ivshmem_attributes,
};

static int rustos_dvm_ivshmem_is_gui_pool(struct pci_dev *pdev)
{
	void __iomem *header;
	void *pixel_header;
	u8 bytes[RUSTOS_GPU_ATLAS_HEADER_OFFSET + RUSTOS_GPU_ATLAS_HEADER_BYTES];
	u8 pixel_bytes[RUSTOS_GPU_ATLAS_HEADER_OFFSET + RUSTOS_GPU_ATLAS_HEADER_BYTES];
	resource_size_t region_bytes;
	int result = -ENODEV;

	region_bytes = pci_resource_len(pdev, RUSTOS_IVSHMEM_SHARED_BAR);
	if (region_bytes < RUSTOS_GUI_POOL_HEADER_BYTES)
		return -ENODEV;
	header = pci_iomap(pdev, RUSTOS_IVSHMEM_SHARED_BAR, sizeof(bytes));
	if (!header)
		return -ENOMEM;
	memcpy_fromio(bytes, header, sizeof(bytes));
	pixel_header = memremap(RUSTOS_GUI_PIXEL_REGION_PHYS,
				sizeof(pixel_bytes), MEMREMAP_WB);
	if (pixel_header) {
		memcpy(pixel_bytes, pixel_header, sizeof(pixel_bytes));
		memunmap(pixel_header);
	} else {
		memset(pixel_bytes, 0, sizeof(pixel_bytes));
	}
	if (!memcmp(bytes, rustos_gui_pool_magic, sizeof(rustos_gui_pool_magic) - 1) &&
	    rustos_dvm_read_le32(bytes + 8) == RUSTOS_GUI_POOL_VERSION &&
	    rustos_dvm_read_le32(bytes + 12) == RUSTOS_GUI_POOL_HEADER_BYTES &&
	    rustos_dvm_read_le32(bytes + 44) == RUSTOS_GUI_POOL_SLOT_COUNT &&
	    rustos_dvm_read_le64(bytes + 16) == RUSTOS_GUI_PIXEL_REGION_BYTES &&
	    !memcmp(bytes, pixel_bytes, RUSTOS_GUI_POOL_RECORD_BYTES) &&
	    !memcmp(bytes + RUSTOS_GPU_ATLAS_HEADER_OFFSET,
		    pixel_bytes + RUSTOS_GPU_ATLAS_HEADER_OFFSET,
		    RUSTOS_GPU_ATLAS_HEADER_BYTES) &&
	    rustos_dvm_valid_gpu_atlas_header(
		    bytes + RUSTOS_GPU_ATLAS_HEADER_OFFSET))
		result = 0;
	pci_iounmap(pdev, header);
	return result;
}

static int rustos_dvm_ivshmem_probe(struct pci_dev *pdev,
				    const struct pci_device_id *id)
{
	struct rustos_dvm_ivshmem *state;
	struct resource *registers;
	struct resource *shared;
	int result;

	result = pcim_enable_device(pdev);
	if (result)
		return result;
	result = rustos_dvm_ivshmem_is_gui_pool(pdev);
	if (result)
		return result;
	if (pci_msix_vec_count(pdev) != RUSTOS_DVM_MSIX_VECTOR_COUNT)
		return -ENODEV;
	result = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_MSIX);
	if (result < 0)
		return result;
	result = devm_add_action_or_reset(&pdev->dev,
					 rustos_dvm_ivshmem_free_vectors, pdev);
	if (result)
		return result;
	shared = &pdev->resource[RUSTOS_IVSHMEM_SHARED_BAR];
	if (!(shared->flags & IORESOURCE_MEM) ||
	    resource_size(shared) < RUSTOS_GUI_POOL_HEADER_BYTES)
		return -ENODEV;
	registers = &pdev->resource[RUSTOS_IVSHMEM_REGISTERS_BAR];
	if (!(registers->flags & IORESOURCE_MEM) ||
	    resource_size(registers) < RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32))
		return -ENODEV;
	state = devm_kzalloc(&pdev->dev, sizeof(*state), GFP_KERNEL);
	if (!state)
		return -ENOMEM;
	if (!devm_request_mem_region(&pdev->dev, RUSTOS_GUI_PIXEL_REGION_PHYS,
				     RUSTOS_GUI_PIXEL_REGION_BYTES,
				     "rustos-dvm-display-pixels"))
		return -EBUSY;
	state->pixel_pgmap.type = MEMORY_DEVICE_GENERIC;
	state->pixel_pgmap.range.start = RUSTOS_GUI_PIXEL_REGION_PHYS;
	state->pixel_pgmap.range.end = RUSTOS_GUI_PIXEL_REGION_PHYS +
		RUSTOS_GUI_PIXEL_REGION_BYTES - 1U;
	state->pixel_pgmap.nr_range = 1;
	state->pixel_pgmap.owner = state;
	state->pixels = devm_memremap_pages(&pdev->dev, &state->pixel_pgmap);
	if (IS_ERR(state->pixels))
		return PTR_ERR(state->pixels);
	state->doorbell = pcim_iomap(pdev, RUSTOS_IVSHMEM_REGISTERS_BAR,
				    RUSTOS_IVSHMEM_DOORBELL_OFFSET + sizeof(u32));
	if (!state->doorbell)
		return -ENOMEM;
	state->shared = pcim_iomap(pdev, RUSTOS_IVSHMEM_SHARED_BAR,
				  RUSTOS_GUI_POOL_HEADER_BYTES);
	if (!state->shared)
		return -ENOMEM;
	state->slot_bytes = readq(state->shared + 48U);
	if (!state->slot_bytes || state->slot_bytes & (PAGE_SIZE - 1) ||
	    state->slot_bytes >
		(RUSTOS_GUI_PIXEL_REGION_BYTES - RUSTOS_GUI_POOL_HEADER_BYTES) /
		RUSTOS_GUI_POOL_SLOT_COUNT)
		return -EPROTO;
	state->gpu_command_offset =
		readq(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 24U);
	state->gpu_atlas_offset =
		readq(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 32U);
	state->gpu_atlas_slot_bytes =
		readq(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 40U);
	state->gpu_atlas_width =
		readl(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 48U);
	state->gpu_atlas_height =
		readl(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 52U);
	state->gpu_atlas_stride =
		readl(state->shared + RUSTOS_GPU_ATLAS_HEADER_OFFSET + 56U);
	atomic_set(&state->host_invited, 0);
	atomic_set(&state->relay_ready, 0);
	atomic_set(&state->gpu_prime_ready, 0);
	atomic64_set(&state->invitation_generation, 0);
	atomic64_set(&state->control_sequence, 0);
	memset(state->acquired_values, 0, sizeof(state->acquired_values));
	mutex_init(&state->control_lock);
	/*
	 * A RustOS present can precede Linux-DVM boot. ivshmem has no retained
	 * interrupt edge, so reconstruct the invitation from its fixed shared
	 * record before userspace opens /dev/uio. This is not a fallback: the same
	 * generation is later matched exactly by the ready sysfs transaction.
	 */
	rustos_dvm_ivshmem_latch_host_invitation(state);
	pci_set_drvdata(pdev, state);
	state->uio.name = RUSTOS_DVM_UIO_NAME;
	state->uio.version = "3";
	state->uio.irq = pci_irq_vector(pdev, 0);
	state->uio.handler = rustos_dvm_ivshmem_irq;
	state->uio.irqcontrol = rustos_dvm_ivshmem_irq_control;
	state->uio.mmap = rustos_dvm_ivshmem_mmap;
	state->uio.mem[0].name = "rustos-gui-surface-pixels-wb-ro";
	state->uio.mem[0].memtype = UIO_MEM_PHYS;
	state->uio.mem[0].addr =
		RUSTOS_GUI_PIXEL_REGION_PHYS + RUSTOS_GUI_POOL_HEADER_BYTES;
	state->uio.mem[0].size =
		RUSTOS_GUI_PIXEL_REGION_BYTES - RUSTOS_GUI_POOL_HEADER_BYTES;
	state->uio.priv = state;
	result = devm_uio_register_device(&pdev->dev, &state->uio);
	if (!result)
		result = devm_device_add_group(&pdev->dev,
					       &rustos_dvm_ivshmem_attribute_group);
	if (!result) {
		state->dmabuf_misc.minor = MISC_DYNAMIC_MINOR;
		state->dmabuf_misc.name = RUSTOS_DVM_DMABUF_NAME;
		state->dmabuf_misc.fops = &rustos_dvm_dmabuf_fops;
		state->dmabuf_misc.parent = &pdev->dev;
		state->dmabuf_misc.mode = 0600;
		result = misc_register(&state->dmabuf_misc);
	}
	if (!result)
		result = devm_add_action_or_reset(&pdev->dev,
						  rustos_dvm_ivshmem_unregister_misc,
						  &state->dmabuf_misc);
	if (!result)
		dev_info(&pdev->dev,
			 "RustOS GUI surface UIO bound: MSI-X vector=%ld control_BAR2=%pa pixels=%pa+%pa\n",
			 (long)state->uio.irq, &shared->start,
			 &state->uio.mem[0].addr, &state->uio.mem[0].size);
	return result;
}

static const struct pci_device_id rustos_dvm_ivshmem_ids[] = {
	{ PCI_DEVICE(RUSTOS_IVSHMEM_VENDOR_ID, RUSTOS_IVSHMEM_DEVICE_ID) },
	{ }
};
MODULE_DEVICE_TABLE(pci, rustos_dvm_ivshmem_ids);

static struct pci_driver rustos_dvm_ivshmem_driver = {
	.name = RUSTOS_DVM_UIO_NAME,
	.id_table = rustos_dvm_ivshmem_ids,
	.probe = rustos_dvm_ivshmem_probe,
};
module_pci_driver(rustos_dvm_ivshmem_driver);

MODULE_AUTHOR("RustOS");
MODULE_DESCRIPTION("RustOS GUI-DVM three-surface MSI-X UIO transport");
MODULE_IMPORT_NS(DMA_BUF);
MODULE_LICENSE("GPL");
