use crate::ipc::{self, KernelSharedRegionHandle};
use crate::memory::paging::UserRegion;
use crate::user::abi::device::{
    DISPLAY_SURFACE_MAX_HEIGHT, DISPLAY_SURFACE_MAX_MAPPING_BYTES,
    DISPLAY_SURFACE_MAX_STRIDE_BYTES, DISPLAY_SURFACE_MAX_WIDTH, PIXEL_FORMAT_BGRA8888,
};

const PAGE_SIZE: u64 = 4096;
const DISPLAY_SURFACE_USAGE_FRAMEBUFFER: u32 = 0;
const DISPLAY_SURFACE_USAGE_GPU_ATLAS: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplaySurfaceHandle {
    width: u32,
    height: u32,
    stride_bytes: u32,
    bytes_per_pixel: u32,
    pixel_format: u32,
    generation: u64,
    usage: u32,
    binding_slot: u32,
    frame_len: u64,
    mapping_len: u64,
    shared_region: Option<KernelSharedRegionHandle>,
    /// Cached higher-half kernel virtual address of `shared_region`'s backing
    /// memory, populated by [`Self::set_shared_region`]. Avoids re-acquiring
    /// the global IPC objects lock (with interrupts disabled) on every present.
    shared_region_kernel_addr: u64,
    shared_region_kernel_len: u64,
    mapped_region: Option<UserRegion>,
}

impl DisplaySurfaceHandle {
    pub fn new(width: u32, height: u32, pixel_format: u32, generation: u64) -> Option<Self> {
        let stride_bytes = width.checked_mul(4)?;
        Self::new_with_stride(width, height, stride_bytes, pixel_format, generation)
    }

    pub fn new_with_stride(
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixel_format: u32,
        generation: u64,
    ) -> Option<Self> {
        Self::new_with_usage(
            width,
            height,
            stride_bytes,
            pixel_format,
            generation,
            DISPLAY_SURFACE_USAGE_FRAMEBUFFER,
            u32::MAX,
        )
    }

    pub fn new_gpu_atlas(
        width: u32,
        height: u32,
        pixel_format: u32,
        generation: u64,
        binding_slot: u32,
    ) -> Option<Self> {
        Self::new_with_usage(
            width,
            height,
            width.checked_mul(4)?,
            pixel_format,
            generation,
            DISPLAY_SURFACE_USAGE_GPU_ATLAS,
            binding_slot,
        )
    }

    fn new_with_usage(
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixel_format: u32,
        generation: u64,
        usage: u32,
        binding_slot: u32,
    ) -> Option<Self> {
        if width == 0
            || height == 0
            || width > DISPLAY_SURFACE_MAX_WIDTH
            || height > DISPLAY_SURFACE_MAX_HEIGHT
            || pixel_format != PIXEL_FORMAT_BGRA8888
            || generation == 0
        {
            return None;
        }

        let bytes_per_pixel = 4_u32;
        let packed_stride = width.checked_mul(bytes_per_pixel)?;
        if stride_bytes < packed_stride
            || stride_bytes > DISPLAY_SURFACE_MAX_STRIDE_BYTES
            || !stride_bytes.is_multiple_of(bytes_per_pixel)
        {
            return None;
        }
        let frame_len = (stride_bytes as u64).checked_mul(height as u64)?;
        if frame_len == 0 || frame_len > DISPLAY_SURFACE_MAX_MAPPING_BYTES {
            return None;
        }
        let mapping_len = align_up(frame_len, PAGE_SIZE)?;

        Some(Self {
            width,
            height,
            stride_bytes,
            bytes_per_pixel,
            pixel_format,
            generation,
            usage,
            binding_slot,
            frame_len,
            mapping_len,
            shared_region: None,
            shared_region_kernel_addr: 0,
            shared_region_kernel_len: 0,
            mapped_region: None,
        })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }

    pub fn stride_bytes(self) -> u32 {
        self.stride_bytes
    }

    pub fn bytes_per_pixel(self) -> u32 {
        self.bytes_per_pixel
    }

    pub fn pixel_format(self) -> u32 {
        self.pixel_format
    }

    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn is_gpu_atlas(self) -> bool {
        self.usage == DISPLAY_SURFACE_USAGE_GPU_ATLAS
    }

    pub fn binding_slot(self) -> Option<u32> {
        self.is_gpu_atlas().then_some(self.binding_slot)
    }

    pub fn frame_len(self) -> u64 {
        self.frame_len
    }

    pub fn mapping_len(self) -> u64 {
        self.mapping_len
    }

    pub fn mapped_region(self) -> Option<UserRegion> {
        self.mapped_region
    }

    pub fn shared_region(self) -> Option<KernelSharedRegionHandle> {
        self.shared_region
    }

    /// Returns the cached `(pointer, len)` for the surface's shared backing
    /// memory. Populated by [`Self::set_shared_region`]. The pointer is valid
    /// for the lifetime of the surface — shared regions are pinned to fixed
    /// physical frames at creation time.
    pub fn shared_region_kernel_mapping(self) -> Option<(*mut u8, usize)> {
        if self.shared_region.is_none() || self.shared_region_kernel_addr == 0 {
            return None;
        }
        Some((
            self.shared_region_kernel_addr as *mut u8,
            self.shared_region_kernel_len as usize,
        ))
    }

    pub fn set_shared_region(&mut self, region: KernelSharedRegionHandle) {
        self.shared_region = Some(region);
        // Cache the kernel-side virtual mapping once so per-present code paths
        // never need to touch the global IPC objects lock. Falling back to
        // a zero address forces callers down the slow lookup path.
        if let Some((ptr, len)) = ipc::map_shared_region(region) {
            self.shared_region_kernel_addr = ptr as u64;
            self.shared_region_kernel_len = len as u64;
        } else {
            self.shared_region_kernel_addr = 0;
            self.shared_region_kernel_len = 0;
        }
    }

    pub fn set_mapped_region(&mut self, region: UserRegion) {
        self.mapped_region = Some(region);
    }

    pub fn clear_mapping(&mut self) {
        self.mapped_region = None;
    }
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| aligned & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::{DisplaySurfaceHandle, PIXEL_FORMAT_BGRA8888};

    #[test]
    fn provider_surface_preserves_padded_stride() {
        let surface =
            DisplaySurfaceHandle::new_with_stride(1600, 900, 7168, PIXEL_FORMAT_BGRA8888, 1)
                .expect("valid padded provider surface");

        assert_eq!(surface.stride_bytes(), 7168);
        assert_eq!(surface.frame_len(), 7168 * 900);
        assert_eq!(surface.mapping_len(), 6_451_200);
    }

    #[test]
    fn provider_surface_rejects_invalid_stride_contracts() {
        assert!(
            DisplaySurfaceHandle::new_with_stride(1600, 900, 6396, PIXEL_FORMAT_BGRA8888, 1,)
                .is_none()
        );
        assert!(
            DisplaySurfaceHandle::new_with_stride(1600, 900, 6402, PIXEL_FORMAT_BGRA8888, 1,)
                .is_none()
        );
    }
}
