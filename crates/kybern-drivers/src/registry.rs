use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kybern_protocol::ProviderKind;

use crate::AgentDriver;

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: HashMap<ProviderKind, Arc<dyn AgentDriver>>,
}

impl DriverRegistry {
    pub fn with_defaults() -> Self {
        Self::with_claude(crate::claude::ClaudeDriver::default())
    }

    /// The default drivers, keeping harness model catalogs in `cache_dir` so a
    /// restarted daemon lists models without waiting on the harness.
    pub fn with_cache_dir(cache_dir: &Path) -> Self {
        Self::with_claude(crate::claude::ClaudeDriver::with_catalog_file(cache_dir.join("claude-models.json")))
    }

    fn with_claude(claude: crate::claude::ClaudeDriver) -> Self {
        let mut r = Self::default();
        r.register(Arc::new(claude));
        r.register(Arc::new(crate::codex::CodexDriver));
        r.register(Arc::new(crate::opencode::OpencodeDriver));
        r.register(Arc::new(crate::pi::PiDriver::pi()));
        r.register(Arc::new(crate::pi::PiDriver::omp()));
        r.register(Arc::new(crate::cursor::CursorDriver));
        r.register(Arc::new(crate::antigravity::AntigravityDriver::default()));
        r.register(Arc::new(crate::grok::GrokDriver));
        r
    }

    pub fn register(&mut self, driver: Arc<dyn AgentDriver>) {
        self.drivers.insert(driver.kind(), driver);
    }

    pub fn get(&self, kind: ProviderKind) -> Option<Arc<dyn AgentDriver>> {
        self.drivers.get(&kind).cloned()
    }

    pub fn kinds(&self) -> impl Iterator<Item = ProviderKind> + '_ {
        self.drivers.keys().copied()
    }
}
