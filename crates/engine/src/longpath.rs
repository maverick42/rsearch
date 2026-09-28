//! Long-path (`\\?\` verbatim) support for filesystem operations.
//!
//! Windows `MAX_PATH` limits legacy path APIs to 260 UTF-16 code units
//! (including the terminating NUL). `File::open`, `metadata` and
//! friends fail on longer paths unless the path carries the verbatim
//! `\\?\` prefix, which bypasses normalization entirely.
//!
//! Policy used here:
//!
//! * Paths **shorter** than `MAX_PATH` are returned unchanged. Verbatim
//!   paths skip normalization (`.`/`..`, trailing dots, forward
//!   slashes), so they are only introduced when actually needed.
//! * Paths at or beyond `MAX_PATH` are converted: `C:\...` becomes
//!   `\\?\C:\...` and `\\server\share\...` becomes
//!   `\\?\UNC\server\share\...`.
//! * Already-verbatim (`\\?\...`) and device-namespace (`\\.\...`)
//!   paths pass through untouched.
//! * A long relative path is first made absolute (lexically, without
//!   touching the filesystem); `\\?\` requires absolute paths.
//!
//! The logical path stored in the index is never rewritten: callers
//! apply [`io_path`] only at the filesystem-call boundary.

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Windows `MAX_PATH` in UTF-16 code units (includes the NUL).
#[cfg(windows)]
const MAX_PATH: usize = 260;

/// Returns a path suitable for filesystem calls: the input unchanged
/// when it is short enough or already verbatim, otherwise a verbatim
/// `\\?\` (or `\\?\UNC\`) path that is not subject to `MAX_PATH`.
///
/// The conversion never decodes path text — it is a pure `OsStr`
/// prefix operation, so non-UTF-8 paths survive unchanged.
pub fn io_path(path: &Path) -> io::Result<PathBuf> {
    io_path_impl(path)
}

/// Opens a file for reading through [`io_path`]. Use this instead of
/// `std::fs::File::open` whenever the path may exceed `MAX_PATH`.
pub fn open(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(io_path(path)?)
}

/// `symlink_metadata` through [`io_path`].
pub fn symlink_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(io_path(path)?)
}

#[cfg(windows)]
fn io_path_impl(path: &Path) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    // Already verbatim or device-namespace: bypasses MAX_PATH as-is.
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        if matches!(
            prefix.kind(),
            Prefix::Verbatim(_)
                | Prefix::VerbatimDisk(_)
                | Prefix::VerbatimUNC(..)
                | Prefix::DeviceNS(_)
        ) {
            return Ok(path.to_path_buf());
        }
    }

    // Paths under MAX_PATH are used exactly as given — verbatim paths
    // skip normalization, so they are only introduced when actually
    // needed. Relative paths stay relative.
    if path.as_os_str().encode_wide().count() < MAX_PATH {
        return Ok(path.to_path_buf());
    }

    // A verbatim path must be absolute. `std::path::absolute` is
    // lexical only: it resolves relative paths against the cwd without
    // touching the filesystem or canonicalizing.
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path)?
    };

    if let Some(Component::Prefix(prefix)) = abs.components().next() {
        match prefix.kind() {
            // \\server\share\... -> \\?\UNC\server\share\...
            Prefix::UNC(server, share) => {
                let mut s = OsString::from(r"\\?\UNC\");
                s.push(server);
                s.push(r"\");
                s.push(share);
                // Skip the Prefix and RootDir components; append the
                // rest verbatim (raw concatenation: `\\?\` paths are
                // never normalized by Windows anyway).
                for comp in abs.components().skip(2) {
                    s.push(r"\");
                    s.push(comp.as_os_str());
                }
                return Ok(PathBuf::from(s));
            }
            Prefix::Verbatim(_)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(..)
            | Prefix::DeviceNS(_) => return Ok(abs),
            _ => {}
        }
    }

    // C:\very\long\... -> \\?\C:\very\long\...  (raw prefix; a verbatim
    // path must not be built through Path::push, which would re-parse).
    let mut s = OsString::from(r"\\?\");
    s.push(abs.as_os_str());
    Ok(PathBuf::from(s))
}

#[cfg(not(windows))]
fn io_path_impl(path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn wide_len(p: &Path) -> usize {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str().encode_wide().count()
    }

    #[test]
    fn short_absolute_path_is_unchanged() {
        let p = Path::new(r"C:\projets\rsearch\file.txt");
        assert_eq!(io_path(p).unwrap(), p);
    }

    #[test]
    fn long_disk_path_gets_verbatim_prefix() {
        let long = PathBuf::from(format!(r"C:\{}\leaf.txt", "a".repeat(300)));
        assert!(wide_len(&long) >= MAX_PATH);
        let io = io_path(&long).unwrap();
        let mut expected = std::ffi::OsString::from(r"\\?\");
        expected.push(long.as_os_str());
        assert_eq!(io, PathBuf::from(expected));
    }

    #[test]
    fn verbatim_path_passes_through() {
        let p = Path::new(r"\\?\C:\already\verbatim\file.txt");
        assert_eq!(io_path(p).unwrap(), p);
    }

    #[test]
    fn short_unc_path_is_unchanged() {
        let p = Path::new(r"\\server\share\dir\file.txt");
        assert_eq!(io_path(p).unwrap(), p);
    }

    #[test]
    fn long_unc_path_becomes_verbatim_unc() {
        let long = PathBuf::from(format!(r"\\server\share\{}\f.txt", "d".repeat(300)));
        let io = io_path(&long).unwrap();
        let expected = format!(r"\\?\UNC\server\share\{}\f.txt", "d".repeat(300));
        assert_eq!(io, PathBuf::from(expected));
    }

    #[test]
    fn short_relative_path_is_unchanged() {
        // A short relative path stays relative: verbatim conversion is
        // only introduced when MAX_PATH is actually exceeded.
        let p = Path::new(r"dir\sub\file.txt");
        assert_eq!(io_path(p).unwrap(), p);
    }

    #[test]
    fn long_relative_path_is_made_absolute_and_verbatim() {
        // 140 path segments -> > 260 code units even relative.
        let rel = (0..140).map(|_| "d").collect::<Vec<_>>().join("\\");
        let io = io_path(Path::new(&rel)).unwrap();
        let s = io.to_str().expect("absolute path is UTF-8");
        assert!(s.starts_with(r"\\?\"), "expected verbatim path, got {s}");
        assert!(io.is_absolute());
    }

    #[test]
    fn already_verbatim_long_path_is_preserved() {
        let p = PathBuf::from(format!(r"\\?\C:\{}\x.txt", "z".repeat(300)));
        assert_eq!(io_path(&p).unwrap(), p);
    }
}
