//! Optional `comicrich.toml` next to the executable.

use std::path::PathBuf;

use serde::Deserialize;

const DEFAULT_APP_ID: &str = "1533059701124632717";
const FILE_NAME: &str = "comicrich.toml";

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Ru,
    En,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app_id: String,
    pub language: Language,
    /// Look comics up in online databases.
    pub online: bool,
    pub metron: MetronConfig,
    pub comicvine: ComicVineConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MetronConfig {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ComicVineConfig {
    pub api_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app_id: DEFAULT_APP_ID.to_string(),
            language: Language::default(),
            online: true,
            metron: MetronConfig::default(),
            comicvine: ComicVineConfig::default(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        Some(std::env::current_exe().ok()?.parent()?.join(FILE_NAME))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&path) else {
            log!("no config at {}, using defaults", path.display());
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(config) => config,
            Err(e) => {
                log!("invalid {}: {e}; using defaults", path.display());
                Self::default()
            }
        }
    }
}
