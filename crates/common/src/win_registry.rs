//! One string out of the registry, read the smallest possible way.
//!
//! Several places need exactly this and nothing more: where a program was
//! installed, what a hypervisor calls this machine. A crate for it would be
//! more than the question deserves, and a second copy of these thirty lines is
//! how two callers end up disagreeing about what an empty value means.

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_READ, REG_VALUE_TYPE,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One string value, or nothing.
///
/// An empty `value` reads the key's DEFAULT value, which is where some
/// installers keep the only thing worth knowing about themselves.
///
/// Nothing is reported: every caller here is asking a question whose honest
/// answer may be "there is no such key", and a missing key is not a failure.
pub fn read_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(root, PCWSTR(wide(subkey).as_ptr()), 0, KEY_READ, &mut key)
            != ERROR_SUCCESS
        {
            return None;
        }
        let name = wide(value);
        let name = if value.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(name.as_ptr())
        };
        let mut kind = REG_VALUE_TYPE(0);
        let mut size: u32 = 0;
        if RegQueryValueExW(key, name, None, Some(&mut kind), None, Some(&mut size))
            != ERROR_SUCCESS
            || size == 0
        {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let read = RegQueryValueExW(
            key,
            name,
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if read != ERROR_SUCCESS {
            return None;
        }
        // The length comes back in BYTES and the string may or may not carry
        // its terminator, so the pairs are taken until one of them is zero.
        let units: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect();
        let text = String::from_utf16_lossy(&units);
        (!text.trim().is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;

    /// A key that is on every Windows there has ever been.
    #[test]
    fn a_value_that_is_always_there_comes_back() {
        let name = read_string(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "ProductName",
        );
        assert!(
            name.is_some_and(|n| n.contains("Windows")),
            "ProductName should say Windows"
        );
    }

    #[test]
    fn a_key_that_is_not_there_is_not_an_error() {
        assert!(read_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\No Such Vendor At All", "").is_none());
    }

    #[test]
    fn a_value_that_is_not_there_is_not_an_error() {
        assert!(read_string(
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "NoSuchValueAtAll"
        )
        .is_none());
    }
}
