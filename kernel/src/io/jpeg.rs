use core::cell::UnsafeCell;

use spin::Mutex;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

const JPEG_DECODE_SCRATCH_CAPACITY: usize = 64 * 1024 * 1024;

#[repr(align(64))]
struct JpegDecodeScratch([u8; JPEG_DECODE_SCRATCH_CAPACITY]);

struct JpegDecodeScratchMemory(UnsafeCell<JpegDecodeScratch>);

unsafe impl Sync for JpegDecodeScratchMemory {}

static JPEG_DECODE_LOCK: Mutex<()> = Mutex::new(());
static JPEG_DECODE_SCRATCH: JpegDecodeScratchMemory = JpegDecodeScratchMemory(UnsafeCell::new(
    JpegDecodeScratch([0; JPEG_DECODE_SCRATCH_CAPACITY]),
));

#[derive(Debug)]
pub enum JpegError {
    DecodeFailed,
    InvalidDimensions,
    OutputTooLarge,
}

pub struct JpegImageView<'a> {
    pub width: usize,
    pub height: usize,
    pub pixels: &'a [u8],
}

pub fn with_decoded_rgb<T>(
    bytes: &[u8],
    visitor: impl FnOnce(JpegImageView<'_>) -> T,
) -> Result<T, JpegError> {
    let _guard = JPEG_DECODE_LOCK.lock();

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(bytes, options);
    decoder
        .decode_headers()
        .map_err(|_| JpegError::DecodeFailed)?;

    let Some(info) = decoder.info() else {
        return Err(JpegError::InvalidDimensions);
    };
    let width = info.width as usize;
    let height = info.height as usize;
    if width == 0 || height == 0 {
        return Err(JpegError::InvalidDimensions);
    }

    let required = decoder
        .output_buffer_size()
        .ok_or(JpegError::OutputTooLarge)?;
    let scratch = unsafe { &mut (*JPEG_DECODE_SCRATCH.0.get()).0 };
    if required > scratch.len() {
        return Err(JpegError::OutputTooLarge);
    }

    let pixels = &mut scratch[..required];
    decoder
        .decode_into(pixels)
        .map_err(|_| JpegError::DecodeFailed)?;

    Ok(visitor(JpegImageView {
        width,
        height,
        pixels,
    }))
}
