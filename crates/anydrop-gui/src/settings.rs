//! Persistent settings: <config dir>/AnyDrop/settings.json
//! (Windows: %APPDATA%\AnyDrop, macOS: ~/Library/Application Support/AnyDrop)

use anydrop_core::DEFAULT_PORT;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    pub download_dir: PathBuf,
    pub port: u16,
    pub auto_accept: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let name = hostname_default();
        let download_dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AnyDrop");
        Self {
            name,
            download_dir,
            port: DEFAULT_PORT,
            auto_accept: false,
        }
    }
}

fn hostname_default() -> String {
    anydrop_core::DeviceInfo::local(None).name
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("AnyDrop").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
