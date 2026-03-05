use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const APP_QUALIFIER: &str = "com.compint";
const APP_NAME: &str = "CI-Applet";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinioConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub use_ssl: bool,
}

impl Default for MinioConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:9000".to_string(),
            bucket: "ci-screens".to_string(),
            region: "us-east-1".to_string(),
            access_key: "minio".to_string(),
            secret_key: "minio123".to_string(),
            use_ssl: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub require_ssl: bool,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "postgres".to_string(),
            database: "ci".to_string(),
            require_ssl: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisConfig {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:1234/v1".to_string(),
            api_key: "lm-studio".to_string(),
            model: "lmstudio-community/vision-large".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IdentityConfig {
    pub user_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub screenshot_interval_secs: u64,
    pub diff_threshold_percent: f32,
    pub storage_dir: PathBuf,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        let default_dir = default_storage_dir().unwrap_or_else(|_| std::env::temp_dir());
        Self {
            screenshot_interval_secs: 300,
            diff_threshold_percent: 10.0,
            storage_dir: default_dir,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VizierConfig {
    pub enabled: bool,
    pub watch_path: Option<PathBuf>,
    pub all_connections: bool,
}

impl Default for VizierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_path: None,
            all_connections: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub capture: CaptureConfig,
    pub minio: MinioConfig,
    pub postgres: PostgresConfig,
    pub analysis: AnalysisConfig,
    pub identity: IdentityConfig,
    pub server: ServerConfig,
    #[serde(default)]
    pub vizier: VizierConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub portal_url: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            portal_url: "https://dev.companionintelligence.com/login".to_string(),
        }
    }
}

pub struct ConfigManager {
    path: PathBuf,
}

impl ConfigManager {
    pub fn new() -> Result<Self> {
        let dir = ensure_config_dir()?;
        Ok(Self {
            path: dir.join("config.json"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_or_default(&self) -> Result<AppConfig> {
        if self.path.exists() {
            let data = fs::read_to_string(&self.path)
                .with_context(|| format!("reading config at {}", self.path.display()))?;
            let config: AppConfig =
                serde_json::from_str(&data).with_context(|| "parsing config".to_string())?;
            Ok(config)
        } else {
            let config = AppConfig::default();
            self.save(&config)?;
            Ok(config)
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let serialized = serde_json::to_string_pretty(config)?;
        fs::write(&self.path, serialized)
            .with_context(|| format!("writing config to {}", self.path.display()))?;
        Ok(())
    }
}

fn ensure_config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating project directory")?;
    let dir = dirs.config_dir();
    fs::create_dir_all(dir)
        .with_context(|| format!("creating config directory {}", dir.display()))?;
    Ok(dir.to_path_buf())
}

pub fn default_storage_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_QUALIFIER, APP_NAME)
        .context("locating storage directory")?;
    let data_dir = dirs.data_dir();
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let screenshot_dir = data_dir.join("screenshots");
    fs::create_dir_all(&screenshot_dir)
        .with_context(|| format!("creating screenshot dir {}", screenshot_dir.display()))?;
    Ok(screenshot_dir)
}
