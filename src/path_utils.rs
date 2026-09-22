use std::path::PathBuf;

/// `std::fs::canonicalize` returns `\\?\` verbatim paths on Windows. Many
/// script launchers and browser-facing APIs do not handle that prefix well,
/// so convert it back to the ordinary DOS/UNC spelling at process boundaries.
pub fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_windows_verbatim_prefix() {
        let input = PathBuf::from(r"\\?\D:\AI\ComfyUI");
        if cfg!(windows) {
            assert_eq!(
                strip_windows_verbatim_prefix(input),
                PathBuf::from(r"D:\AI\ComfyUI")
            );
        }
    }
}
