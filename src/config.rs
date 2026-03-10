use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub backend: BackendConfig,
    pub audio: AudioConfig,
    pub injection: InjectionConfig,
    pub postprocessing: PostprocessingConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct BackendConfig {
    pub url: String,
    pub frame_threshold: u32,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub device: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct InjectionConfig {
    /// Ordered list of drivers to try. Default: ["wtype", "dotool", "clipboard"]
    pub driver_order: Vec<String>,
    /// Paste keystroke for PasteInjector. Default: "ctrl+shift+v"
    pub paste_keys: String,
    /// App IDs that trigger paste mode instead of direct typing.
    pub terminal_app_ids: Vec<String>,
    /// Enable Niri IPC detection for auto-switching between terminal and default chains.
    pub niri_detect: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PostprocessingConfig {
    pub hallucination_filter: bool,
    pub spoken_punctuation: bool,
}


impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            url: "ws://localhost:8000/asr".into(),
            frame_threshold: 25,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: "default".into(),
        }
    }
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            driver_order: vec!["wtype".into(), "dotool".into(), "clipboard".into()],
            paste_keys: "ctrl+shift+v".into(),
            terminal_app_ids: vec!["kitty".into(), "foot".into(), "Alacritty".into()],
            niri_detect: true,
        }
    }
}

impl Default for PostprocessingConfig {
    fn default() -> Self {
        Self {
            hallucination_filter: true,
            spoken_punctuation: true,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("asr-rs").join("config.toml"))
}

pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        tracing::info!("no XDG config dir found, using defaults");
        return Ok(Config::default());
    };
    if !path.exists() {
        tracing::info!("no config at {}, using defaults", path.display());
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&text)?;
    tracing::info!("loaded config from {}", path.display());
    Ok(config)
}
