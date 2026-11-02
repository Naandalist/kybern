use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub db: PathBuf,
    pub token_file: PathBuf,
    pub port_file: PathBuf,
    pub worktrees: PathBuf,
    pub assets: PathBuf,
    pub settings: PathBuf,
}

/// `~/.kybern`, the data folder when none is given.
pub fn default_data_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".kybern"))
}

impl Paths {
    pub fn resolve(override_root: Option<PathBuf>) -> Result<Self> {
        let root = match override_root {
            Some(r) => r,
            None => default_data_dir().context("no home directory")?,
        };
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            db: root.join("state.sqlite"),
            token_file: root.join("daemon.token"),
            port_file: root.join("daemon.port"),
            worktrees: root.join("worktrees"),
            assets: root.join("assets"),
            settings: root.join("settings.json"),
            root,
        })
    }
}
