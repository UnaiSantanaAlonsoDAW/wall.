//! Explicit per-user registration for a portable executable. Querying this
//! module never changes Windows settings, including after the binary moves.
use std::{env, os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{Foundation::*, System::Registry::*},
    core::{PCWSTR, w},
};

const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("Wall.WallpaperSwitcher");

fn command(path: &Path) -> Vec<u16> {
    let mut text = vec![u16::from(b'"')];
    text.extend(path.as_os_str().encode_wide());
    text.extend("\" --background\0".encode_utf16());
    text
}

pub fn enabled() -> Result<bool, String> {
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    matches_registration(HKEY_CURRENT_USER, RUN, &command(&exe))
}

fn matches_registration(root: HKEY, key: PCWSTR, expected: &[u16]) -> Result<bool, String> {
    // The supported Run command length is 260 characters. A longer value
    // cannot match any registration we could write.
    let mut buffer = [0u16; 261];
    let mut bytes = size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            root,
            key,
            VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    match status {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_MORE_DATA => Ok(false),
        _ => {
            status
                .ok()
                .map_err(|e| format!("Cannot read Windows startup preference: {e}"))?;
            Ok(buffer.get(..bytes as usize / 2) == Some(expected))
        }
    }
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    write_registration(HKEY_CURRENT_USER, RUN, &command(&exe), enabled)
}

fn write_registration(
    root: HKEY,
    key: PCWSTR,
    command: &[u16],
    enabled: bool,
) -> Result<(), String> {
    let status = if enabled {
        if command.len() > 261 {
            return Err("The executable path is too long for Windows startup. Move wall. to a shorter path and try again.".into());
        }
        unsafe {
            RegSetKeyValueW(
                root,
                key,
                VALUE,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                size_of_val(command) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(root, key, VALUE) }
    };
    if !enabled && matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        return Ok(());
    }
    status
        .ok()
        .map_err(|e| format!("Cannot update Windows startup preference: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_portable_command_and_registration_round_trip() {
        // Volatile isolated HKCU key, never the user's actual Run key.
        let name: Vec<u16> = format!(
            "Software\\WallTest-{}-{}\0",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
        .encode_utf16()
        .collect();
        let key = PCWSTR(name.as_ptr());
        let mut handle = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key,
                None,
                PCWSTR::null(),
                REG_OPTION_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut handle,
                None,
            )
        }
        .ok()
        .unwrap();
        struct Cleanup<'a>(HKEY, &'a [u16]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                unsafe {
                    let _ = RegCloseKey(self.0);
                    let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(self.1.as_ptr()));
                }
            }
        }
        let _cleanup = Cleanup(handle, &name);
        let original = command(Path::new(r"C:\My apps\そら\wall.exe"));
        let moved = command(Path::new(r"D:\My apps\wall.exe"));
        assert_eq!(
            String::from_utf16(&original).unwrap(),
            "\"C:\\My apps\\そら\\wall.exe\" --background\0"
        );
        let registered = |cmd: &[u16]| matches_registration(handle, PCWSTR::null(), cmd).unwrap();
        let set = |cmd: &[u16], on| write_registration(handle, PCWSTR::null(), cmd, on).unwrap();
        assert!(!registered(&original));
        set(&original, true);
        assert!(registered(&original));
        assert!(!registered(&moved));
        set(&moved, true);
        assert!(registered(&moved));
        assert!(!registered(&original));
        set(&moved, false);
        set(&moved, false);
        assert!(!registered(&moved));
    }
}
