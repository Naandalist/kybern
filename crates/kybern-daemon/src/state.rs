use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use kybern_drivers::registry::DriverRegistry;
use kybern_protocol::{ProviderStatus, ThreadEvent};
use kybern_store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::access::Pairing;
use crate::config::Paths;
use crate::orchestrator::Orchestrator;
use crate::settings::SettingsStore;
use crate::terminal::TerminalManager;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub paths: Paths,
    pub store: Store,
    pub drivers: DriverRegistry,
    pub events: crate::bounded_broadcast::Sender<ThreadEvent>,
    pub orchestrator: Orchestrator,
    pub native_tools: crate::native_tools_mcp::NativeToolsGateway,
    pub terminals: TerminalManager,
    pub file_indexes: crate::files::FileIndexCache,
    pub settings: SettingsStore,
    pub provider_catalogs: Arc<ProviderCatalogCache>,
    pub thread_projections: crate::thread_projection::ThreadProjectionCache,
    pub harness_updates: crate::harness_updates::HarnessUpdates,
    pub daemon_updates: crate::self_update::DaemonUpdates,
    /// Updates of the desktop app that manages this daemon (ADE-125): its attached windows and a pending request.
    pub app_updates: crate::app_update::AppUpdates,
    pub cursor_setup: crate::cursor_setup::CursorSetup,
    pub account_identities: crate::account_identity::AccountIdentities,
    pub account_logins: crate::account_login::AccountLogins,
    /// First-run scan and bulk import of saved harness sessions.
    pub session_imports: crate::session_import::SessionImports,
    /// Where this machine's user skill folders are (ADE-86 skill copies).
    pub plugin_runtime: crate::plugins::PluginRuntime,
    pub skill_homes: crate::network::skill_sync::SkillHomes,
    /// Preview file and proxy tickets (ADE-34).
    pub previews: Arc<crate::previews::tickets::PreviewTickets>,
    /// Started by the desktop app, whose bundle owns this binary.
    pub desktop_managed: std::sync::atomic::AtomicBool,
    /// This state belongs to a `kybernd` process (`run`), not to a test harness: the process may be
    /// ended outright when a graceful shutdown that it asked for hangs.
    pub process_owned: std::sync::atomic::AtomicBool,
    /// A newer binary is in place; hand over to it after the graceful shutdown.
    pub restart_pending: std::sync::atomic::AtomicBool,
    /// The binary to start after that shutdown when it is not this process's own path (an updated install moved it).
    pub restart_exe: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// "Keep this machine reachable when Kybern is closed": the login item that starts this daemon.
    pub login_service: crate::login_service::LoginService,
    pub pairing: Pairing,
    pub tickets: crate::access::Tickets,
    pub revoked_tokens: broadcast::Sender<uuid::Uuid>,
    pub listen_addr: std::sync::RwLock<Option<std::net::SocketAddr>>,
    pub exposure: crate::exposure::Exposure,
    /// The machine network: this machine's identity and membership, and discovery and sessions with
    /// the other members (ADE-86).
    pub network: Arc<crate::network::Network>,
    pub advertised_urls: std::sync::RwLock<Vec<String>>,
    pub port: std::sync::atomic::AtomicU16,
    pub bootstrap_token: String,
    /// The bootstrap token's id in the store: how the local desktop app is recognized
    /// (`auth::is_bootstrap`).
    pub bootstrap_token_id: uuid::Uuid,
    pub environment_id: String,
    pub started_at: DateTime<Utc>,
    pub shutdown: CancellationToken,
    /// Open WebSocket connections. Maintained by `ws::run`.
    pub connections: std::sync::atomic::AtomicUsize,
    /// When the daemon last had no clients, sessions, terminals, or queued
    /// work; `None` while anything is active. Maintained by the maintenance sweep.
    pub idle_since: std::sync::RwLock<Option<DateTime<Utc>>>,
    /// Latest host power probe; `None` until the first sweep or on hosts
    /// that cannot report it. Maintained by the maintenance sweep.
    pub on_battery: std::sync::RwLock<Option<bool>>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

impl AppState {
    /// Whether `background.save_power_on_battery` applies right now.
    pub fn saving_power(&self) -> bool {
        self.settings.get().background.save_power_on_battery
            && *self.on_battery.read().unwrap_or_else(|poisoned| poisoned.into_inner()) == Some(true)
    }

    /// Construct daemon state before restart recovery. The desktop cold-start
    /// handshake binds its socket after this step, then recovery completes
    /// before axum begins accepting requests.
    pub fn initialize(paths: &Paths) -> Result<Self> {
        Self::initialize_with(paths, crate::network::tailscale::from_env(), crate::network::Timing::default())
    }

    /// [`AppState::initialize`] with the Tailscale seam and the network's intervals chosen by the
    /// caller: the multi-daemon harness passes a fixture-driven fake and short intervals.
    pub fn initialize_with(paths: &Paths, tailscale: Arc<dyn crate::network::Tailscale>, timing: crate::network::Timing) -> Result<Self> {
        Self::initialize_with_drivers(paths, tailscale, timing, DriverRegistry::with_cache_dir(&paths.root.join("cache")))
    }

    /// [`AppState::initialize_with`] with the harness drivers chosen by the caller (the multi-daemon
    /// harness registers scripted ones).
    pub fn initialize_with_drivers(
        paths: &Paths,
        tailscale: Arc<dyn crate::network::Tailscale>,
        timing: crate::network::Timing,
        drivers: DriverRegistry,
    ) -> Result<Self> {
        Self::initialize_with_store(paths, tailscale, timing, drivers, Store::open(&paths.db)?)
    }

    #[cfg(test)]
    pub(crate) fn initialize_in_memory(paths: &Paths) -> Result<Self> {
        Self::initialize_with_store(
            paths,
            crate::network::tailscale::from_env(),
            crate::network::Timing::default(),
            DriverRegistry::with_cache_dir(&paths.root.join("cache")),
            Store::open_in_memory()?,
        )
    }

    fn initialize_with_store(
        paths: &Paths,
        tailscale: Arc<dyn crate::network::Tailscale>,
        timing: crate::network::Timing,
        drivers: DriverRegistry,
        store: Store,
    ) -> Result<Self> {
        let crate::auth::Bootstrap { token: bootstrap_token, id: bootstrap_token_id } =
            crate::auth::ensure_bootstrap(&store, &paths.token_file)?;
        let environment_id = match store.meta_get("environment_id")? {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::now_v7().to_string();
                store.meta_set("environment_id", &id)?;
                id
            }
        };
        let (events, _) = crate::bounded_broadcast::channel(8192, 8 * 1024 * 1024);
        // Secret store (S3): move plaintext provider keys out of settings.json before it is read.
        crate::secret_store::start(&paths.root, &paths.settings, &store);
        let settings = SettingsStore::load(&paths.settings)?;
        settings.attach_stamps(store.clone());
        let harness_updates = crate::harness_updates::HarnessUpdates::new(&store)?;
        let daemon_updates = crate::self_update::DaemonUpdates::new(&store)?;
        let terminals = TerminalManager::default();
        let native_tools = crate::native_tools_mcp::NativeToolsGateway::default();
        let previews = Arc::new(crate::previews::tickets::PreviewTickets::new());
        // The network comes first: remote delegation reaches other machines through it.
        let network = Arc::new(crate::network::Network::new(&paths.root, &store, &environment_id, tailscale, timing));
        let shutdown = CancellationToken::new();
        let hub = crate::network::outbox::NetworkHub::new(network.clone(), shutdown.clone());
        // The login broker (S4) calls other machines through the same hub.
        let login_broker = crate::network::broker::LoginBroker::new(hub.clone(), network.clone(), store.clone());
        let orchestrator = Orchestrator::new(store.clone(), drivers.clone(), events.clone(), paths.clone(), settings.clone())
            .with_terminal_manager(terminals.clone())
            .with_previews(previews.clone())
            .with_native_tools(native_tools.clone())
            .with_remote_hub(hub)
            .with_login_broker(login_broker.clone());
        orchestrator.usage().set_bound_reader(login_broker);
        let provider_catalogs = orchestrator.provider_catalog_cache();
        network.set_enabled(settings.get().network.enabled);
        let revoked_tokens = broadcast::channel(128).0;
        network.attach_revocations(revoked_tokens.clone());
        // Other machines learn from presence whether this one's computer tools are ready.
        let feature_network = network.clone();
        orchestrator.computer().set_offer_sink(Arc::new(move |on| {
            feature_network.set_dynamic_feature(kybern_protocol::features::COMPUTER_USE_V1, on);
        }));
        // A device revoked while this daemon was down, but already in its operation set, stays revoked.
        network.revoke_revoked_devices();
        // Clients paired before the network existed hold the network scopes too (N4).
        crate::access::upgrade_paired_tokens(&store);
        Ok(Self {
            inner: Arc::new(Inner {
                paths: paths.clone(),
                store,
                drivers,
                events,
                orchestrator,
                native_tools,
                terminals,
                file_indexes: crate::files::FileIndexCache::default(),
                settings,
                provider_catalogs,
                thread_projections: crate::thread_projection::ThreadProjectionCache::default(),
                harness_updates,
                daemon_updates,
                app_updates: Default::default(),
                cursor_setup: Default::default(),
                account_identities: Default::default(),
                account_logins: Default::default(),
                session_imports: Default::default(),
                skill_homes: Default::default(),
                plugin_runtime: Default::default(),
                previews,
                desktop_managed: std::sync::atomic::AtomicBool::new(false),
                process_owned: std::sync::atomic::AtomicBool::new(false),
                restart_pending: std::sync::atomic::AtomicBool::new(false),
                restart_exe: std::sync::Mutex::new(None),
                login_service: Default::default(),
                pairing: Pairing::default(),
                tickets: crate::access::Tickets::default(),
                revoked_tokens,
                listen_addr: std::sync::RwLock::new(None),
                exposure: Default::default(),
                network,
                advertised_urls: std::sync::RwLock::new(vec![]),
                port: std::sync::atomic::AtomicU16::new(0),
                bootstrap_token,
                bootstrap_token_id,
                environment_id,
                started_at: Utc::now(),
                shutdown,
                connections: std::sync::atomic::AtomicUsize::new(0),
                idle_since: std::sync::RwLock::new(None),
                on_battery: std::sync::RwLock::new(None),
            }),
        })
    }
}

const PROVIDER_CATALOG_TTL: Duration = Duration::from_secs(60);

struct CachedProviderCatalog {
    providers: Vec<ProviderStatus>,
    refreshed_at: Instant,
}

const MAX_PROVIDER_CATALOGS: usize = 32;

/// Keeps expensive, harness-owned model discovery off repeated app boots.
///
/// The cache key includes project context and provider settings. A single
/// refresh lock also coalesces concurrent desktop/CLI requests without adding
/// any provider-specific policy to the daemon.

#[derive(Default)]
pub struct ProviderCatalogCache {
    entries: Mutex<HashMap<String, CachedProviderCatalog>>,
    refresh: Mutex<()>,
    last_known: std::sync::Mutex<LastKnown>,
}

/// Most projects whose last catalog is kept on disk.
const MAX_KNOWN_CATALOGS: usize = 16;

/// The last catalog that listed models for a project, kept across restarts so a
/// slow probe can still be answered (see [`ProviderCatalogCache::last_known`]).
#[derive(Default)]
struct LastKnown {
    file: Option<PathBuf>,
    loaded: bool,
    projects: HashMap<String, KnownCatalog>,
}

#[derive(Clone, Serialize, Deserialize)]
struct KnownCatalog {
    saved_at: DateTime<Utc>,
    providers: Vec<ProviderStatus>,
}

#[derive(Default, Serialize, Deserialize)]
struct KnownCatalogsFile {
    #[serde(default)]
    projects: HashMap<String, KnownCatalog>,
}

/// Write `saved` to `file` through a temporary file, so a crash never leaves half of it.
fn write_known_catalogs(file: &std::path::Path, saved: &KnownCatalogsFile) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = file.with_extension(format!("json.tmp-{}", uuid::Uuid::now_v7().simple()));
    std::fs::write(&temporary, serde_json::to_vec(saved)?)?;
    std::fs::rename(&temporary, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

impl ProviderCatalogCache {
    /// A cache that also keeps each project's last good catalog in `file`.
    pub fn with_disk(file: PathBuf) -> Self {
        Self { last_known: std::sync::Mutex::new(LastKnown { file: Some(file), ..Default::default() }), ..Default::default() }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, LastKnown> {
        let mut known = self.last_known.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !known.loaded {
            known.loaded = true;
            if let Some(file) = known.file.clone() {
                match std::fs::read(&file) {
                    Ok(bytes) => match serde_json::from_slice::<KnownCatalogsFile>(&bytes) {
                        Ok(saved) => known.projects = saved.projects,
                        Err(error) => tracing::warn!(%error, file = %file.display(), "ignoring an unreadable provider catalog cache"),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => tracing::warn!(%error, file = %file.display(), "could not read the provider catalog cache"),
                }
            }
        }
        known
    }

    /// Keep `providers` as the project's last good catalog. A probe that found
    /// no harness with models (everything still loading, or all failed) never
    /// replaces a catalog that had some.
    pub fn remember(&self, project_key: &str, providers: &[ProviderStatus]) {
        if !providers.iter().any(|status| status.available && !status.models.is_empty()) {
            return;
        }
        let (file, saved) = {
            let mut known = self.known();
            known.projects.insert(project_key.to_owned(), KnownCatalog { saved_at: Utc::now(), providers: providers.to_vec() });
            while known.projects.len() > MAX_KNOWN_CATALOGS {
                let Some(oldest) = known.projects.iter().min_by_key(|(_, catalog)| catalog.saved_at).map(|(key, _)| key.clone()) else {
                    break;
                };
                known.projects.remove(&oldest);
            }
            (known.file.clone(), KnownCatalogsFile { projects: known.projects.clone() })
        };
        let Some(file) = file else { return };
        let write = move || {
            if let Err(error) = write_known_catalogs(&file, &saved) {
                tracing::warn!(%error, file = %file.display(), "could not save the provider catalog cache");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(write);
            }
            Err(_) => write(),
        }
    }

    /// The project's last good catalog and when it was taken, from this run or
    /// an earlier one.
    pub fn last_known(&self, project_key: &str) -> Option<(Vec<ProviderStatus>, DateTime<Utc>)> {
        self.known().projects.get(project_key).map(|catalog| (catalog.providers.clone(), catalog.saved_at))
    }

    pub async fn invalidate(&self) {
        self.entries.lock().await.clear();
    }
    pub async fn get_if_fresh(&self, key: &str) -> Option<Vec<ProviderStatus>> {
        self.fresh(key).await
    }
    pub async fn get_or_refresh<F, Fut>(&self, key: String, force_refresh: bool, refresh: F) -> Vec<ProviderStatus>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Vec<ProviderStatus>>,
    {
        let requested_at = Instant::now();
        if !force_refresh && let Some(providers) = self.fresh(&key).await {
            return providers;
        }

        let _refresh = self.refresh.lock().await;
        if force_refresh {
            // A concurrent request already refreshed this exact account after
            // this call began. Share its result instead of probing it again.
            if let Some(entry) = self.entries.lock().await.get(&key)
                && entry.refreshed_at >= requested_at
            {
                return entry.providers.clone();
            }
        } else if let Some(providers) = self.fresh(&key).await {
            return providers;
        }

        let providers = refresh().await;
        let mut entries = self.entries.lock().await;
        entries.retain(|_, entry| entry.refreshed_at.elapsed() < PROVIDER_CATALOG_TTL);
        if !entries.contains_key(&key)
            && entries.len() >= MAX_PROVIDER_CATALOGS
            && let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.refreshed_at).map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(key, CachedProviderCatalog { providers: providers.clone(), refreshed_at: Instant::now() });
        providers
    }

    async fn fresh(&self, key: &str) -> Option<Vec<ProviderStatus>> {
        self.entries
            .lock()
            .await
            .get(key)
            .filter(|entry| entry.refreshed_at.elapsed() < PROVIDER_CATALOG_TTL)
            .map(|entry| entry.providers.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn provider_catalog_refreshes_are_coalesced() {
        let cache = ProviderCatalogCache::default();
        let calls = AtomicUsize::new(0);
        let load = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Vec::new()
        };

        let (first, second) =
            tokio::join!(cache.get_or_refresh("global".into(), false, load), cache.get_or_refresh("global".into(), false, load));

        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_catalog_cache_evicts_oldest_project() {
        let cache = ProviderCatalogCache::default();
        for index in 0..=MAX_PROVIDER_CATALOGS {
            cache.get_or_refresh(index.to_string(), false, || async { Vec::new() }).await;
        }
        assert_eq!(cache.entries.lock().await.len(), MAX_PROVIDER_CATALOGS);
        assert!(cache.fresh("0").await.is_none());
        assert!(cache.fresh(&MAX_PROVIDER_CATALOGS.to_string()).await.is_some());
        let calls = AtomicUsize::new(0);
        cache
            .get_or_refresh("0".into(), false, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.entries.lock().await.len(), MAX_PROVIDER_CATALOGS);
    }

    #[tokio::test]
    async fn forced_provider_catalog_refresh_bypasses_cache() {
        let cache = ProviderCatalogCache::default();
        let calls = AtomicUsize::new(0);
        let mut load = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        };

        cache.get_or_refresh("global".into(), false, &mut load).await;
        cache.get_or_refresh("global".into(), true, &mut load).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn simultaneous_forced_account_catalog_refreshes_share_one_native_probe() {
        let cache = ProviderCatalogCache::default();
        let calls = AtomicUsize::new(0);
        let entered = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let first = cache.get_or_refresh("named-account".into(), true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            release.notified().await;
            Vec::new()
        });
        let second = async {
            entered.notified().await;
            let second = cache.get_or_refresh("named-account".into(), true, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            });
            tokio::pin!(second);
            // Poll into the refresh lock before allowing the first probe to end.
            assert!(futures::poll!(&mut second).is_pending());
            release.notify_one();
            second.await
        };
        let _ = tokio::join!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
