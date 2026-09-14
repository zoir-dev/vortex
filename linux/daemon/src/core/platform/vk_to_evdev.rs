//! Windows virtual-key codes → Linux evdev keycodes.
//!
//! [`super::InputEvent::Key`] carries an evdev code because the phone already
//! speaks them: the Android side injects through the same numbering the Linux
//! uinput path uses, and inventing a third space would mean a translation table
//! on the phone as well. So Windows translates here and the wire stays
//! identical between platforms.
//!
//! Compiled everywhere so it can be tested here. A wrong entry is a key that
//! types the wrong character on the phone — silent, and invisible until someone
//! presses that key.
//!
//! # Coverage
//!
//! The main block, the modifiers, the navigation cluster, F1–F12 and the numpad.
//! Deliberately NOT exhaustive: media keys, browser keys, IME keys and the
//! OEM-specific extras are absent, and [`vk_to_evdev`] returns `None` for them
//! so the caller drops the event rather than sending a plausible-looking wrong
//! code. Layout-dependent VKs (`VK_OEM_*`) map by their US-layout position,
//! which is what evdev codes mean anyway — a keycode names a physical key, and
//! the phone applies its own layout on top.

/// Letter keys, indexed by `vk - 0x41` (`VK_A`..`VK_Z`).
///
/// A table rather than arithmetic: evdev numbers keys by physical row
/// (`KEY_Q` = 16 because Q is the first key of the top letter row), so the
/// alphabet is not contiguous there the way it is in the VK space.
const LETTERS: [u16; 26] = [
    30, // A
    48, // B
    46, // C
    32, // D
    18, // E
    33, // F
    34, // G
    35, // H
    23, // I
    36, // J
    37, // K
    38, // L
    50, // M
    49, // N
    24, // O
    25, // P
    16, // Q
    19, // R
    31, // S
    20, // T
    22, // U
    47, // V
    17, // W
    45, // X
    21, // Y
    44, // Z
];

/// Digit row, indexed by `vk - 0x30` (`VK_0`..`VK_9`).
///
/// `KEY_1` is 2 and `KEY_0` is 11 — the row starts at 1, not 0, so zero sits at
/// the END. Getting this backwards is the classic off-by-one here.
const DIGITS: [u16; 10] = [
    11, // 0
    2,  // 1
    3, 4, 5, 6, 7, 8, 9, 10,
];

/// Translate one virtual-key code. `None` means "not mapped" — see the coverage
/// note; the caller must drop it rather than guess.
pub fn vk_to_evdev(vk: u16) -> Option<u16> {
    Some(match vk {
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        // F1..F10 are contiguous in both spaces; F11/F12 are not (evdev puts
        // the numpad block between them).
        0x70..=0x79 => 59 + (vk - 0x70),
        0x7A => 87, // F11
        0x7B => 88, // F12

        0x1B => 1,  // Escape
        0x08 => 14, // Backspace
        0x09 => 15, // Tab
        0x0D => 28, // Enter
        0x20 => 57, // Space
        0x14 => 58, // CapsLock

        // Modifiers. The sided VKs are what a low-level hook reports; the
        // unsided VK_SHIFT/CONTROL/MENU arrive from other APIs and are mapped
        // to the left-hand key, which is the conventional choice.
        0xA0 | 0x10 => 42,  // LeftShift / Shift
        0xA1 => 54,         // RightShift
        0xA2 | 0x11 => 29,  // LeftCtrl / Ctrl
        0xA3 => 97,         // RightCtrl
        0xA4 | 0x12 => 56,  // LeftAlt / Alt
        0xA5 => 100,        // RightAlt (AltGr)
        0x5B => 125,        // LeftWin  → KEY_LEFTMETA
        0x5C => 126,        // RightWin → KEY_RIGHTMETA
        0x5D => 127,        // Apps     → KEY_COMPOSE

        // Navigation cluster.
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x24 => 102, // Home
        0x23 => 107, // End
        0x21 => 104, // PageUp
        0x22 => 109, // PageDown
        0x2D => 110, // Insert
        0x2E => 111, // Delete

        // Punctuation, by US-layout position.
        0xBD => 12, // -
        0xBB => 13, // =
        0xDB => 26, // [
        0xDD => 27, // ]
        0xBA => 39, // ;
        0xDE => 40, // '
        0xC0 => 41, // `
        0xDC => 43, // backslash
        0xBC => 51, // ,
        0xBE => 52, // .
        0xBF => 53, // /

        // Numpad. Distinct from the digit row on purpose: the phone's injector
        // treats them as separate keys, and NumLock changes what they produce.
        0x60 => 82, // KP0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73, // KP9
        0x6A => 55, // KP*
        0x6B => 78, // KP+
        0x6D => 74, // KP-
        0x6E => 83, // KP.
        0x6F => 98, // KP/
        0x90 => 69, // NumLock
        0x91 => 70, // ScrollLock

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Spot-check against the evdev header. These four are the ones worth
    /// pinning by hand: the alphabet's row ordering and the digit row's offset
    /// are where a plausible-looking table goes wrong.
    #[test]
    fn known_codes_match_the_evdev_header() {
        assert_eq!(vk_to_evdev(0x41), Some(30)); // A → KEY_A
        assert_eq!(vk_to_evdev(0x51), Some(16)); // Q → KEY_Q (first of its row)
        assert_eq!(vk_to_evdev(0x5A), Some(44)); // Z → KEY_Z
        assert_eq!(vk_to_evdev(0x31), Some(2)); //  1 → KEY_1
        assert_eq!(vk_to_evdev(0x30), Some(11)); // 0 → KEY_0, at the row's END
        assert_eq!(vk_to_evdev(0x0D), Some(28)); // Enter
        assert_eq!(vk_to_evdev(0x1B), Some(1)); //  Escape
    }

    #[test]
    fn the_letter_row_order_is_qwerty_not_alphabetical() {
        // Q W E R T Y should be consecutive evdev codes 16..21.
        let qwerty = [0x51u16, 0x57, 0x45, 0x52, 0x54, 0x59];
        let codes: Vec<u16> = qwerty.iter().map(|v| vk_to_evdev(*v).unwrap()).collect();
        assert_eq!(codes, vec![16, 17, 18, 19, 20, 21]);
    }

    #[test]
    fn f_keys_are_contiguous_except_where_evdev_is_not() {
        assert_eq!(vk_to_evdev(0x70), Some(59)); // F1
        assert_eq!(vk_to_evdev(0x79), Some(68)); // F10
        // F11/F12 jump past the numpad block rather than continuing at 69.
        assert_eq!(vk_to_evdev(0x7A), Some(87));
        assert_eq!(vk_to_evdev(0x7B), Some(88));
        assert_ne!(vk_to_evdev(0x7A), Some(69));
    }

    /// Two keys mapping to one code means one of them types the wrong thing.
    /// The sided/unsided modifier aliases are the only intended collisions.
    #[test]
    fn no_unintended_collisions() {
        let unsided = [0x10u16, 0x11, 0x12];
        let mut seen: HashSet<u16> = HashSet::new();
        for vk in 0u16..=0xFF {
            if unsided.contains(&vk) {
                continue;
            }
            if let Some(code) = vk_to_evdev(vk) {
                assert!(seen.insert(code), "vk {vk:#04x} duplicates evdev {code}");
            }
        }
    }

    /// The unsided aliases must land on the LEFT-hand key, not somewhere else.
    #[test]
    fn unsided_modifiers_alias_the_left_key() {
        assert_eq!(vk_to_evdev(0x10), vk_to_evdev(0xA0)); // Shift → LeftShift
        assert_eq!(vk_to_evdev(0x11), vk_to_evdev(0xA2)); // Ctrl  → LeftCtrl
        assert_eq!(vk_to_evdev(0x12), vk_to_evdev(0xA4)); // Alt   → LeftAlt
    }

    /// The numpad digits must NOT collide with the digit row: they are separate
    /// physical keys and NumLock changes what they mean.
    #[test]
    fn the_numpad_is_distinct_from_the_digit_row() {
        for (row, pad) in [(0x30u16, 0x60u16), (0x31, 0x61), (0x39, 0x69)] {
            assert_ne!(vk_to_evdev(row), vk_to_evdev(pad), "vk {row:#04x}/{pad:#04x}");
        }
    }

    /// Unmapped means unmapped: no zero codes (evdev 0 is KEY_RESERVED) and no
    /// silent fallback for keys we do not handle.
    #[test]
    fn unmapped_keys_return_none_and_nothing_maps_to_zero() {
        assert_eq!(vk_to_evdev(0xB0), None); // media next-track
        assert_eq!(vk_to_evdev(0xAD), None); // volume mute
        assert_eq!(vk_to_evdev(0x00), None);
        for vk in 0u16..=0xFF {
            assert_ne!(vk_to_evdev(vk), Some(0), "vk {vk:#04x} mapped to KEY_RESERVED");
        }
    }
}
