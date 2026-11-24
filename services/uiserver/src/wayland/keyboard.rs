//! wl_keyboard keymap, key repeat and modifier state.
//!
//! The compositor owns one fully expanded XKB keymap (no `include` lines, so a
//! client needs no xkeyboard-config data) in a memfd that every `wl_keyboard`
//! receives. Key codes are evdev codes, which clients offset by 8 to get XKB
//! keycodes. Modifier state is derived from the post-event state that the
//! keyboard driver reports with every key event.

use std::os::fd::{AsFd, BorrowedFd};

use keyboard_core::Modifiers;

use crate::sys::debug_line;

use super::*;

/// `xkbcli compile-keymap --layout us --model pc105`, regenerate with the same
/// command. Real modifier bit order is the XKB one: Shift, Lock, Control,
/// Mod1 (Alt), Mod2 (NumLock), Mod3, Mod4 (Super), Mod5.
const KEYMAP_US: &str = include_str!("keymap_us.xkb");

/// Clients repeat held keys themselves: the input source does not synthesize
/// autorepeat, and `keyboard_input` drops any repeat it does forward.
const KEY_REPEAT_RATE_PER_SECOND: i32 = 25;
const KEY_REPEAT_DELAY_MS: i32 = 600;

const XKB_SHIFT: u32 = 1 << 0;
const XKB_LOCK: u32 = 1 << 1;
const XKB_CONTROL: u32 = 1 << 2;
const XKB_MOD1_ALT: u32 = 1 << 3;
const XKB_MOD2_NUM_LOCK: u32 = 1 << 4;
const XKB_MOD4_SUPER: u32 = 1 << 6;

pub(super) struct KeymapFile {
    fd: OwnedFd,
    size: u32,
}

impl KeymapFile {
    fn create() -> Result<Self, i32> {
        let name = CString::new("rustos-wl-keymap").map_err(|_| libc::EINVAL)?;
        // SAFETY: `name` is a valid NUL-terminated string for the call.
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
        }
        // SAFETY: `raw` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // The protocol requires the keymap to be NUL terminated and counted in size.
        let mut bytes = Vec::with_capacity(KEYMAP_US.len() + 1);
        bytes.extend_from_slice(KEYMAP_US.as_bytes());
        bytes.push(0);
        let mut written = 0;
        while written < bytes.len() {
            // SAFETY: the pointer/length pair stays inside `bytes`.
            let count = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                )
            };
            if count <= 0 {
                return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            }
            written += count as usize;
        }
        let size = u32::try_from(bytes.len()).map_err(|_| libc::EOVERFLOW)?;
        Ok(Self { fd, size })
    }

    fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// `(depressed, locked)` XKB modifier masks for the driver's modifier bits.
pub(super) fn xkb_modifier_masks(bits: u32) -> (u32, u32) {
    let modifiers = Modifiers::from_bits_truncate(bits as u8);
    let mut depressed = 0;
    if modifiers.contains(Modifiers::SHIFT) {
        depressed |= XKB_SHIFT;
    }
    if modifiers.contains(Modifiers::CTRL) {
        depressed |= XKB_CONTROL;
    }
    if modifiers.contains(Modifiers::ALT) {
        depressed |= XKB_MOD1_ALT;
    }
    if modifiers.contains(Modifiers::META) {
        depressed |= XKB_MOD4_SUPER;
    }
    let mut locked = 0;
    if modifiers.contains(Modifiers::CAPS_LOCK) {
        locked |= XKB_LOCK;
    }
    if modifiers.contains(Modifiers::NUM_LOCK) {
        locked |= XKB_MOD2_NUM_LOCK;
    }
    (depressed, locked)
}

impl WaylandState {
    /// Build the shared keymap once. `false` means no keyboard can be offered.
    pub(super) fn ensure_keymap(&mut self) -> bool {
        if self.keymap.is_some() {
            return true;
        }
        match KeymapFile::create() {
            Ok(keymap) => {
                self.keymap = Some(keymap);
                true
            }
            Err(errno) => {
                debug_line(&format!(
                    "uiserver: wayland keymap unavailable errno={errno}; keyboard not advertised"
                ));
                false
            }
        }
    }

    /// Mandatory first events of a new `wl_keyboard`, then a late `enter` when
    /// the client already owns the keyboard focus.
    pub(super) fn announce_keyboard(&mut self, keyboard: &wl_keyboard::WlKeyboard, client_id: &ClientId) {
        if let Some(keymap) = self.keymap.as_ref() {
            keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1, keymap.fd(), keymap.size);
        }
        if keyboard.version() >= 4 {
            keyboard.repeat_info(KEY_REPEAT_RATE_PER_SECOND, KEY_REPEAT_DELAY_MS);
        }
        let Some(focus) = self
            .keyboard_focus
            .as_ref()
            .filter(|focus| &focus.client_id == client_id)
            .map(|focus| focus.surface.clone())
        else {
            return;
        };
        let serial = self.next_configure_serial();
        self.record_input_serial(serial, client_id.clone());
        let (depressed, locked) = self.keyboard_mods;
        keyboard.enter(serial, &focus, Vec::<u8>::new());
        keyboard.modifiers(serial, depressed, 0, locked, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_is_a_self_contained_xkb_text_keymap() {
        assert!(KEYMAP_US.starts_with("xkb_keymap {"));
        assert!(!KEYMAP_US.contains("include "), "clients have no xkeyboard-config data");
        for section in ["xkb_keycodes", "xkb_types", "xkb_compat", "xkb_symbols"] {
            assert!(KEYMAP_US.contains(section), "missing {section}");
        }
    }

    #[test]
    fn keymap_memfd_holds_the_text_plus_one_nul() {
        let keymap = KeymapFile::create().expect("memfd keymap");
        assert_eq!(keymap.size as usize, KEYMAP_US.len() + 1);
        let mut back = vec![0_u8; keymap.size as usize];
        // SAFETY: the buffer is exactly `size` bytes.
        let count = unsafe {
            libc::pread(keymap.fd.as_raw_fd(), back.as_mut_ptr().cast(), back.len(), 0)
        };
        assert_eq!(count as usize, back.len());
        assert_eq!(&back[..KEYMAP_US.len()], KEYMAP_US.as_bytes());
        assert_eq!(back[KEYMAP_US.len()], 0);
    }

    #[test]
    fn driver_modifiers_map_to_xkb_masks() {
        assert_eq!(xkb_modifier_masks(0), (0, 0));
        let held = (Modifiers::SHIFT | Modifiers::CTRL | Modifiers::ALT | Modifiers::META).bits();
        assert_eq!(
            xkb_modifier_masks(u32::from(held)),
            (XKB_SHIFT | XKB_CONTROL | XKB_MOD1_ALT | XKB_MOD4_SUPER, 0)
        );
        let locks = (Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK | Modifiers::SCROLL_LOCK).bits();
        assert_eq!(xkb_modifier_masks(u32::from(locks)), (0, XKB_LOCK | XKB_MOD2_NUM_LOCK));
    }
}
