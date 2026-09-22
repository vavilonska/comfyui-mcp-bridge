use std::{fs, io, path::PathBuf};

const CIVITAI_VERSION: &str = "0.1.102";
const CIVITAI_EXE: &[u8] = include_bytes!("../vendor/civitai/civitai.exe");
const CIVITAI_LICENSE: &str = include_str!("../vendor/civitai/LICENSE");

/// Extract the bundled, Apache-2.0 Civitai CLI into the portable data folder.
/// A versioned filename avoids overwriting an executable used by another
/// process while still allowing a newer app build to carry a newer CLI.
pub fn ensure_bundled_civitai(data_dir: &std::path::Path) -> io::Result<PathBuf> {
    let bin_dir = data_dir.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let path = bin_dir.join(format!("civitai-{CIVITAI_VERSION}.exe"));
    let current_matches = fs::read(&path)
        .map(|bytes| bytes == CIVITAI_EXE)
        .unwrap_or(false);
    if !current_matches {
        fs::write(&path, CIVITAI_EXE)?;
    }
    Ok(path)
}

pub fn bundled_civitai_version() -> &'static str {
    CIVITAI_VERSION
}

pub fn bundled_civitai_license() -> &'static str {
    CIVITAI_LICENSE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_cli_extracts_as_a_nonempty_exe() {
        let temp = tempfile::tempdir().unwrap();
        let path = ensure_bundled_civitai(temp.path()).unwrap();
        assert!(path.is_file());
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".exe")
        );
        assert_eq!(fs::metadata(path).unwrap().len(), CIVITAI_EXE.len() as u64);
    }
}
