//! Opens a file with its Windows file association.
//!
//! Windows is the source of truth: [`ShellExecuteW`] with the "open"
//! verb resolves the per-extension association exactly like a
//! double-click in Explorer — rsearch never maps extensions to
//! applications itself. The path is passed as null-terminated UTF-16
//! data, never interpolated into a command line.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// Why Windows refused an open — mapped to user-facing messages by
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellOpenError {
    /// No application is associated with the file's extension
    /// (`SE_ERR_NOASSOC`).
    NoAssociation,
    /// The file or its path no longer exists (`ERROR_*_NOT_FOUND`).
    NotFound,
    /// Any other refusal — the raw `ShellExecuteW` code (< 32) is
    /// kept for the banner's detail.
    Failed(i32),
}

/// `SW_SHOWNORMAL` — the default window state, like Explorer's open.
const SW_SHOWNORMAL: i32 = 1;

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> isize;
}

/// Null-terminated UTF-16 for a `ShellExecuteW` string argument.
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Maps the `ShellExecuteW` result code: greater than 32 is success,
/// the documented small values are the `SE_ERR_*`/`ERROR_*` refusals.
fn map_shell_code(code: i32) -> Result<(), ShellOpenError> {
    match code {
        c if c > 32 => Ok(()),
        2 | 3 => Err(ShellOpenError::NotFound),
        31 => Err(ShellOpenError::NoAssociation),
        other => Err(ShellOpenError::Failed(other)),
    }
}

/// Asks Windows to open `path` with the application currently
/// associated with its extension. Returns the refusal reason when
/// Windows cannot open it; rsearch itself never launches an
/// application by name.
pub fn open_with_association(path: &Path) -> Result<(), ShellOpenError> {
    let file = wide(path.as_os_str());
    let operation = wide(OsStr::new("open"));
    // SAFETY: every string argument is a null-terminated UTF-16
    // buffer that outlives the call; the null hwnd means "no parent
    // window", the null parameters/directory mean "none".
    let code = unsafe {
        ShellExecuteW(
            0,
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as i32;
    map_shell_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_codes_map_to_refusal_kinds() {
        assert_eq!(map_shell_code(33), Ok(()));
        assert_eq!(map_shell_code(42), Ok(()));
        assert_eq!(map_shell_code(31), Err(ShellOpenError::NoAssociation));
        assert_eq!(map_shell_code(2), Err(ShellOpenError::NotFound));
        assert_eq!(map_shell_code(3), Err(ShellOpenError::NotFound));
        assert_eq!(map_shell_code(0), Err(ShellOpenError::Failed(0)));
        assert_eq!(map_shell_code(5), Err(ShellOpenError::Failed(5)));
        // 32 is SE_ERR_DLLNOTFOUND — still a refusal.
        assert_eq!(map_shell_code(32), Err(ShellOpenError::Failed(32)));
    }

    #[test]
    fn wide_strings_are_null_terminated_utf16() {
        assert_eq!(wide(OsStr::new("open")), vec![0x6F, 0x70, 0x65, 0x6E, 0]);
        // Spaces, parentheses and accents survive as data — they are
        // never passed through a shell.
        let payload = wide(OsStr::new("a b(c)é"));
        assert_eq!(payload.last(), Some(&0));
        assert_eq!(
            &payload[..payload.len() - 1],
            OsStr::new("a b(c)é").encode_wide().collect::<Vec<_>>().as_slice()
        );
    }
}
