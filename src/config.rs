//! Configuration loading for `lafd`.

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Runtime configuration. Missing fields fall back to [`Config::default`].
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Unix socket used by `lafdctl`. Empty means `$XDG_RUNTIME_DIR/lafd.sock`.
    pub socket_path: String,
    /// Case-insensitive substrings matched against the bus name and identity.
    /// Matching players are ignored entirely. `playerctld` is always ignored.
    pub ignore: Vec<String>,
    /// Case-insensitive substrings that mark a player as a trigger. A trigger
    /// player that starts playing pauses everything until it stops.
    pub trigger_players: Vec<String>,
    /// Default maximum trigger duration in milliseconds. Zero means no limit.
    pub trigger_timeout_ms: u64,
    /// Window in milliseconds during which a resumed player is protected from
    /// being paused again by another player's playback transition.
    pub resume_grace_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            ignore: vec!["playerctld".to_string()],
            trigger_players: Vec::new(),
            trigger_timeout_ms: 0,
            resume_grace_ms: 1500,
        }
    }
}

/// `$XDG_RUNTIME_DIR/lafd.sock`, or `/tmp/lafd.sock` as a fallback.
pub fn default_socket_path() -> String {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => format!("{}/lafd.sock", dir.trim_end_matches('/')),
        _ => "/tmp/lafd.sock".to_string(),
    }
}

/// `$XDG_CONFIG_HOME/lafd/config.json` or `~/.config/lafd/config.json`.
pub fn default_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(Path::new(&dir).join("lafd/config.json"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(&home).join(".config/lafd/config.json"))
}

impl Config {
    /// Load a config file. A missing file is not an error.
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let mut cfg = Self::default();
        let Some(path) = path.map(Path::to_path_buf).or_else(default_config_path) else {
            return Ok(cfg);
        };

        match fs::read_to_string(&path) {
            Ok(data) => {
                cfg = serde_json::from_str(&data)
                    .map_err(|err| format!("parse {}: {err}", path.display()))?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("read {}: {err}", path.display())),
        }

        if cfg.socket_path.is_empty() {
            cfg.socket_path = default_socket_path();
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_usable() {
        let cfg = Config::default();
        assert!(!cfg.socket_path.is_empty());
        assert_eq!(cfg.ignore, vec!["playerctld".to_string()]);
        assert_eq!(cfg.resume_grace_ms, 1500);
        assert_eq!(cfg.trigger_timeout_ms, 0);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let path = std::env::temp_dir().join("lafd-does-not-exist/config.json");
        let cfg = Config::load(Some(&path)).expect("load defaults");
        assert!(!cfg.socket_path.is_empty());
    }

    #[test]
    fn overrides_are_applied() {
        let dir = std::env::temp_dir().join(format!("lafd-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{
                "socket_path": "/tmp/lafd-test.sock",
                "ignore": ["playerctld", "vlc"],
                "trigger_players": ["alarm"],
                "trigger_timeout_ms": 30000,
                "resume_grace_ms": 250
            }"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&path)).expect("load config");
        assert_eq!(cfg.socket_path, "/tmp/lafd-test.sock");
        assert_eq!(cfg.ignore, vec!["playerctld", "vlc"]);
        assert_eq!(cfg.trigger_players, vec!["alarm"]);
        assert_eq!(cfg.trigger_timeout_ms, 30000);
        assert_eq!(cfg.resume_grace_ms, 250);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_json_is_reported() {
        let dir = std::env::temp_dir().join(format!("lafd-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(Config::load(Some(&path)).is_err());
        fs::remove_dir_all(&dir).ok();
    }
}
