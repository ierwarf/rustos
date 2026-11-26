//! Validated, borrowed argument/environment strings for a synchronous request.
//!
//! The request stays borrowed through broker commit. No CString allocation or
//! payload copy is needed on either image path; the broker copies user strings before
//! returning, and no pointer is retained across the next receive.
use alloc::vec::Vec;
use core::ffi::CStr;

const EINVAL: i32 = 22;

#[derive(Clone, Copy, Debug)]
pub struct RequestStrings<'a> {
    bytes: &'a [u8],
}

impl<'a> RequestStrings<'a> {
    pub fn parse(bytes: &'a [u8], len: usize, count: usize) -> Result<Self, i32> {
        let bytes = bytes.get(..len).ok_or(EINVAL)?;
        // Every admitted item contains at least one non-NUL byte and its NUL.
        // Reject impossible counts without allocation or an untrusted capacity.
        if count > len / 2 {
            return Err(EINVAL);
        }
        let mut observed = 0usize;
        for item in bytes.split_inclusive(|byte| *byte == 0) {
            if item.len() <= 1 || CStr::from_bytes_with_nul(item).is_err() {
                return Err(EINVAL);
            }
            observed += 1;
        }
        if observed != count {
            return Err(EINVAL);
        }
        Ok(Self { bytes })
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a CStr> + '_ {
        self.bytes.split_inclusive(|byte| *byte == 0).map(|item| {
            // SAFETY: parse checked every complete item and the private slice
            // is immutable for the lifetime of this view.
            unsafe { CStr::from_bytes_with_nul_unchecked(item) }
        })
    }

    /// PE runtime strings require UTF-8; retain only borrowed views instead
    /// of allocating a CString and copying every argument before conversion.
    pub fn utf8(&self) -> Result<Vec<&'a str>, i32> {
        self.iter().map(|value| value.to_str().map_err(|_| EINVAL)).collect()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{ffi::CString, vec::Vec};

    fn original(bytes: &[u8], len: usize, count: usize) -> Result<Vec<CString>, i32> {
        if len > bytes.len() {
            return Err(EINVAL);
        }
        if count == 0 {
            return (len == 0).then(Vec::new).ok_or(EINVAL);
        }
        let mut values = Vec::with_capacity(count);
        let mut start = 0;
        for index in 0..len {
            if bytes[index] != 0 {
                continue;
            }
            if start == index {
                return Err(EINVAL);
            }
            values.push(CString::new(&bytes[start..index]).map_err(|_| EINVAL)?);
            start = index + 1;
        }
        if start != len || values.len() != count {
            return Err(EINVAL);
        }
        Ok(values)
    }

    #[test]
    fn borrowed_parser_preserves_the_original_acceptance_and_payload() {
        for size in 0..=6_u32 {
            for mut pattern in 0..3_usize.pow(size) {
                let mut bytes = [0_u8; 6];
                for byte in bytes.iter_mut().take(size as usize) {
                    *byte = [0, b'a', b'b'][pattern % 3];
                    pattern /= 3;
                }
                for len in 0..=7 {
                    for count in 0..=4 {
                        let old = original(&bytes[..size as usize], len, count);
                        let new = RequestStrings::parse(&bytes[..size as usize], len, count);
                        assert_eq!(
                            old.is_ok(),
                            new.is_ok(),
                            "{bytes:?} len={len} count={count}"
                        );
                        if let (Ok(old), Ok(new)) = (old, new) {
                            assert!(old.iter().map(|s| s.as_c_str()).eq(new.iter()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn strings_borrow_the_received_buffer_including_terminators() {
        let bytes = *b"app\0KEY=value\0unused";
        let strings = RequestStrings::parse(&bytes, 14, 2).unwrap();
        let mut values = strings.iter();
        assert_eq!(values.next().unwrap().as_ptr().cast::<u8>(), bytes.as_ptr());
        assert_eq!(
            values.next().unwrap().as_ptr().cast::<u8>(),
            bytes[4..].as_ptr()
        );
        assert!(values.next().is_none());
        assert_eq!(
            RequestStrings::parse(&bytes, bytes.len(), usize::MAX).unwrap_err(),
            EINVAL
        );
    }

    #[test]
    fn pe_utf8_views_borrow_and_do_not_restrict_elf_byte_strings() {
        let bytes = b"app\0NAME=value\0";
        let strings = RequestStrings::parse(bytes, bytes.len(), 2).unwrap();
        let views = strings.utf8().unwrap();
        assert_eq!(views, ["app", "NAME=value"]);
        assert_eq!(views[1].as_ptr(), bytes[4..].as_ptr());
        let raw = RequestStrings::parse(b"\xff\0", 2, 1).unwrap();
        assert_eq!(raw.iter().next().unwrap().to_bytes(), b"\xff");
        assert_eq!(raw.utf8(), Err(EINVAL));
    }
}
