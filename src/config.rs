use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use rand::RngCore;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 8765;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub mcp_enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub access_token: String,
    pub comfyui_path: Option<PathBuf>,
    pub comfyui_url: String,
    pub civitai_cli_path: Option<PathBuf>,
    pub allow_custom_download_dirs: bool,
    pub allowed_download_roots: Vec<PathBuf>,
    pub auto_open_panel: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mcp_enabled: true,
            bind_address: "0.0.0.0".to_owned(),
            port: DEFAULT_PORT,
            access_token: generate_token(),
            comfyui_path: None,
            comfyui_url: String::new(),
            civitai_cli_path: None,
            allow_custom_download_dirs: false,
            allowed_download_roots: Vec::new(),
            auto_open_panel: true,
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        if path.is_file() {
            let text = fs::read_to_string(path)?;
            let mut config: Self = serde_json::from_str(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if config.access_token.trim().is_empty() {
                config.access_token = generate_token();
            }
            config.validate()?;
            return Ok(config);
        }

        let config = Self::default();
        config.save(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|error| io::Error::other(error.to_string()))?;
        fs::write(path, bytes)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "port must be between 1 and 65535",
            ));
        }
        if self.bind_address.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bind_address cannot be empty",
            ));
        }
        if self.access_token.len() < 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "access_token must contain at least 32 characters",
            ));
        }
        Ok(())
    }

    pub fn regenerate_token(&mut self) -> String {
        self.access_token = generate_token();
        self.access_token.clone()
    }
}

pub fn default_data_dir() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("COMFYUI_MCP_DATA_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    #[cfg(debug_assertions)]
    {
        Ok(env::current_dir()?.join("data"))
    }

    #[cfg(not(debug_assertions))]
    {
        let exe = env::current_exe()?;
        let parent = exe
            .parent()
            .ok_or_else(|| io::Error::other("application executable has no parent directory"))?;
        Ok(parent.join("data"))
    }
}

pub fn generate_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_strong_token_and_lan_binding() {
        let config = AppConfig::default();
        assert_eq!(config.access_token.len(), 64);
        assert_eq!(config.bind_address, "0.0.0.0");
        assert!(config.mcp_enabled);
        assert!(config.comfyui_url.is_empty());
    }

    #[test]
    fn config_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let expected = AppConfig::default();
        expected.save(&path).unwrap();
        let actual = AppConfig::load_or_create(&path).unwrap();
        assert_eq!(actual.access_token, expected.access_token);
        assert_eq!(actual.port, expected.port);
    }
}
