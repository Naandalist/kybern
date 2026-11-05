//! Account plan limits (5-hour, weekly, monthly) per provider.
//!
//! Limits belong to the account, not to a thread, so one cache serves every
//! client. Three sources feed it, newest wins per window:
//!
//! - live reads: each provider's turn-free probe (Claude's OAuth usage
//!   endpoint, Codex `account/rateLimits/read`, Cursor's dashboard usage). A read runs when a
//!   client asks and the cached value is older than the provider's freshness
//!   window, and shortly after a turn on that provider ends, since that is when
//!   usage moves;
//! - session reports: limits a running turn reports, folded in as they arrive.
//!   Claude reports utilization only near a limit, so these are a supplement;
//! - the event log: the last stored report, shown only until the first read so
//!   a freshly started daemon has something to show.
//!
//! The last good values are also kept in `<data dir>/cache/usage-limits.json`
//! (written atomically a moment after each change), so the rings survive a
//! restart. Values loaded from that file are served as `stale: from_last_run`
//! until a live read succeeds, and a failed read keeps serving them with the
//! reason it failed. A provider with cached values is never hidden.
//!
//! `usage.limits` can answer from the cache at once; every change, and every
//! finished read, goes to clients as `usage.limits.changed`. Reads happen only
//! while a client has asked recently, so an unattended daemon spawns nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use kybern_drivers::claude::UsageUnread;
use kybern_protocol::methods::{LimitsSource, LimitsStale, ProviderLimits, UsageLimitsParams, UsageLimitsResult};
use kybern_protocol::{ProviderInstance, ProviderKind, UsageLimit};
use kybern_store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::settings::SettingsStore;

/// Providers with a turn-free way to read the account's limits. Grok Build has none: `grok` reports
/// cost per turn only, so it is not listed and its limits read as unavailable.
const LIVE: [ProviderKind; 4] = [ProviderKind::ClaudeCode, ProviderKind::Codex, ProviderKind::Cursor, ProviderKind::Antigravity];
/// Forced reads closer together than this share one result.
const MIN_SPACING: Duration = Duration::from_secs(10);
/// A turn ending refreshes its provider only if a client asked this recently.
const INTEREST: Duration = Duration::from_secs(10 * 60);
/// Upper bound for callers that wait for reads (older clients, the CLI).
const WAIT: Duration = Duration::from_secs(16);
/// Changes within this window share one write of the cache file.
const PERSIST_DELAY: Duration = Duration::from_secs(2);

/// How long a live read stays fresh. Claude and Cursor are HTTPS requests
/// (Claude falls back to starting Claude Code for `/usage`); Codex starts its
/// app-server for about a second.
fn fresh_for(_kind: ProviderKind) -> Duration {
    Duration::from_secs(60)
}

/// Wait after a turn ends before reading. Cursor's billing lands a few seconds
/// after the run; the harnesses update their own counters immediately.
fn settle_after_turn(kind: ProviderKind) -> Duration {
    match kind {
        ProviderKind::Cursor => Duration::from_secs(8),
        _ => Duration::from_secs(2),
    }
}

#[derive(Clone)]
pub struct UsageMonitor {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    settings: SettingsStore,
    state: std::sync::Mutex<State>,
    /// One read per provider at a time; a caller that arrives mid-read waits
    /// for it and then finds the value fresh.
    gates: HashMap<ProviderKind, tokio::sync::Mutex<()>>,
    /// The same, per named account. Reads happen only for accounts a client
    /// asked about, so an idle window spawns nothing for them.
    account_gates: std::sync::Mutex<HashMap<AccountKey, Arc<tokio::sync::Mutex<()>>>>,
    changed: broadcast::Sender<UsageLimitsResult>,
    /// Reads the limits of broker-bound accounts, once the network installed it.
    bound: std::sync::Mutex<Option<Arc<dyn BoundUsage>>>,
    /// Where the last good limits are kept between runs.
    cache_file: PathBuf,
    persist_delay: std::sync::Mutex<Duration>,
    /// The bytes last written, so an unchanged cache is not written again.
    last_written: std::sync::Mutex<Option<Vec<u8>>>,
}

type AccountKey = (ProviderKind, String);

/// Plan limits of an account that signs in through the login broker (S4): it has no login on this
/// machine to read, so the broker that holds the real one reads them.
#[async_trait::async_trait]
pub trait BoundUsage: Send + Sync {
    /// `None` when the account is not bound to the broker (read it locally).
    async fn read(&self, kind: ProviderKind, instance: &str) -> Option<Result<(Vec<UsageLimit>, Option<String>), UsageUnread>>;
}

#[derive(Default)]
struct State {
    accounts: BTreeMap<AccountKey, Entry>,
    accounts_refreshing: HashSet<AccountKey>,
    seeded: bool,
    /// A write of the cache file is waiting for its delay to pass.
    persist_scheduled: bool,
    account_identities: BTreeMap<ProviderKind, String>,
    /// The global account each identity above was taken for.
    global_accounts: BTreeMap<ProviderKind, String>,
    providers: BTreeMap<ProviderKind, Entry>,
    last_interest: Option<Instant>,
    refreshing: HashSet<ProviderKind>,
    after_turn: HashSet<ProviderKind>,
}

#[derive(Default)]
struct Entry {
    limits: Vec<UsageLimit>,
    plan: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    source: Option<LimitsSource>,
    /// Last live read, successful or not. Paces retries for a provider that
    /// is not installed or signed in.
    attempted: Option<Instant>,
    /// Why the last live read failed, until one succeeds.
    stale: Option<(LimitsStale, Option<DateTime<Utc>>)>,
    /// The failure last written to the log, so a read failing the same way
    /// every minute is logged once.
    logged: Option<String>,
}

fn window_key(limit: &UsageLimit) -> String {
    limit.window_minutes.map(|minutes| format!("window:{minutes}")).unwrap_or_else(|| format!("name:{}", limit.name))
}

fn entry_limits(kind: ProviderKind, instance: Option<String>, entry: &Entry, now: i64) -> ProviderLimits {
    ProviderLimits {
        provider: kind,
        limits: entry
            .limits
            .iter()
            .map(|limit| match limit.resets_at {
                Some(reset) if reset <= now => UsageLimit { used_percent: 0.0, ..limit.clone() },
                _ => limit.clone(),
            })
            .collect(),
        updated_at: entry.updated_at,
        source: entry.source,
        plan: entry.plan.clone(),
        stale: entry.stale.map(|(reason, _)| reason),
        retry_at: entry.stale.and_then(|(_, retry_at)| retry_at),
        instance,
    }
}

/// Record the outcome of a live read. A failed read keeps the last values and
/// says why they are old; a successful one makes them current.
fn apply_read(entry: &mut Entry, kind: ProviderKind, read: Result<(Vec<UsageLimit>, Option<String>), UsageUnread>) {
    entry.attempted = Some(Instant::now());
    match read {
        Ok((limits, plan)) => {
            entry.limits = limits;
            entry.plan = plan.or(entry.plan.take());
            entry.updated_at = Some(Utc::now());
            entry.source = Some(LimitsSource::Live);
            entry.stale = None;
            if entry.logged.take().is_some() {
                tracing::info!(provider = %kind, "account limits read again");
            }
        }
        Err(unread) => {
            if entry.logged.as_deref() != Some(unread.detail.as_str()) {
                tracing::info!(provider = %kind, reason = ?unread.reason, detail = %unread.detail, "could not read account limits; keeping the last values");
                entry.logged = Some(unread.detail);
            }
            entry.stale = Some((unread.reason, unread.retry_at));
        }
    }
}

/// The cache file: the last good limits per provider and named account.
#[derive(Default, Serialize, Deserialize)]
struct SavedLimits {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    providers: Vec<SavedProvider>,
    #[serde(default)]
    accounts: Vec<SavedAccount>,
}

#[derive(Serialize, Deserialize)]
struct SavedProvider {
    provider: ProviderKind,
    /// The account, binary and environment the values were read for.
    identity: String,
    #[serde(flatten)]
    entry: SavedEntry,
}

#[derive(Serialize, Deserialize)]
struct SavedAccount {
    provider: ProviderKind,
    instance: String,
    #[serde(flatten)]
    entry: SavedEntry,
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    limits: Vec<UsageLimit>,
    #[serde(default)]
    plan: Option<String>,
    updated_at: DateTime<Utc>,
    source: LimitsSource,
}

impl SavedEntry {
    fn from_entry(entry: &Entry) -> Option<Self> {
        let source = entry.source.filter(|source| *source != LimitsSource::Stored)?;
        if entry.limits.is_empty() {
            return None;
        }
        Some(Self { limits: entry.limits.clone(), plan: entry.plan.clone(), updated_at: entry.updated_at?, source })
    }

    /// Serve these as the last run's values, with their original time.
    fn load_into(self, entry: &mut Entry) {
        entry.limits = self.limits;
        entry.plan = self.plan;
        entry.updated_at = Some(self.updated_at);
        entry.source = Some(self.source);
        entry.stale = Some((LimitsStale::FromLastRun, None));
    }
}

/// Write through a temporary file in the same directory, so a crash never leaves half a file.
fn write_atomic(file: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = file.with_extension(format!("json.tmp-{}", uuid::Uuid::now_v7().simple()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn provider_order(kind: ProviderKind) -> usize {
    LIVE.iter().position(|live| *live == kind).unwrap_or(LIVE.len())
}

impl UsageMonitor {
    pub fn new(store: Store, settings: SettingsStore) -> Self {
        let cache_file = settings.dir().join("cache").join("usage-limits.json");
        Self {
            inner: Arc::new(Inner {
                cache_file,
                persist_delay: std::sync::Mutex::new(PERSIST_DELAY),
                last_written: std::sync::Mutex::new(None),
                store,
                settings,
                state: std::sync::Mutex::new(State::default()),
                gates: LIVE.into_iter().map(|kind| (kind, tokio::sync::Mutex::new(()))).collect(),
                account_gates: Default::default(),
                changed: broadcast::channel(64).0,
                bound: std::sync::Mutex::new(None),
            }),
        }
    }

    /// Let broker-bound accounts read their limits from the broker.
    pub fn set_bound_reader(&self, reader: Arc<dyn BoundUsage>) {
        *self.inner.bound.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reader);
    }

    fn identity(&self, kind: ProviderKind) -> String {
        let settings = self.inner.settings.get();
        let raw = settings.providers.get(&kind).cloned().unwrap_or_default();
        self.identity_for(kind, &crate::provider_accounts::resolve(&raw, None, None))
    }

    /// The identity global limits would have if `account` were the default
    /// under today's binary and environment.
    fn identity_for(&self, kind: ProviderKind, account: &str) -> String {
        let mut settings = self.inner.settings.get();
        settings.providers.entry(kind).or_default().default_account = (account != "default").then(|| account.to_string());
        let account = account.to_string();
        let effective = crate::settings::provider_settings(&settings, kind, None);
        let bytes = Sha256::digest(serde_json::to_vec(&(account, effective.binary, effective.env)).unwrap_or_default());
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Global limits describe the selected global account. Never show the
    /// previous account's values or accept its in-flight read after a switch.
    ///
    /// When only the default account changed (same binary and environment),
    /// the limits swap places: the previous default keeps its values as a
    /// named-account entry and the new default starts from its own, so a
    /// surface showing the default never goes blank until the next read.
    fn sync_accounts(&self) {
        let settings = self.inner.settings.get();
        let identities = LIVE
            .into_iter()
            .map(|kind| {
                let raw = settings.providers.get(&kind).cloned().unwrap_or_default();
                (kind, self.identity(kind), crate::provider_accounts::resolve(&raw, None, None))
            })
            .collect::<Vec<_>>();
        let mut state = self.lock();
        for (kind, identity, account) in identities {
            if state.account_identities.get(&kind).is_some_and(|old| old != &identity) {
                let previous = state.providers.remove(&kind);
                let previous_account = state.global_accounts.get(&kind).cloned();
                let switched_only = previous_account.as_ref().is_some_and(|old| {
                    old != &account && self.identity_for(kind, old) == *state.account_identities.get(&kind).unwrap_or(&String::new())
                });
                if switched_only {
                    if let (Some(entry), Some(old)) = (previous, previous_account) {
                        state.accounts.insert((kind, old), entry);
                    }
                    if let Some(entry) = state.accounts.remove(&(kind, account.clone())) {
                        state.providers.insert(kind, entry);
                    }
                }
            }
            state.account_identities.insert(kind, identity);
            state.global_accounts.insert(kind, account);
        }
    }

    pub fn settings_changed(&self) {
        self.sync_accounts();
        self.publish();
    }

    pub fn observe_account(&self, provider: &ProviderInstance, limits: &[UsageLimit]) {
        self.seed();
        let settings = self.inner.settings.get();
        let raw = settings.providers.get(&provider.kind).cloned().unwrap_or_default();
        if provider.instance == crate::provider_accounts::resolve(&raw, None, None) {
            self.observe(provider.kind, limits);
        } else if LIVE.contains(&provider.kind) && !limits.is_empty() {
            {
                let mut state = self.lock();
                let entry = state.accounts.entry((provider.kind, provider.instance.clone())).or_default();
                for limit in limits {
                    let key = window_key(limit);
                    match entry.limits.iter_mut().find(|old| window_key(old) == key) {
                        Some(old) => {
                            old.used_percent = limit.used_percent;
                            old.resets_at = limit.resets_at.or(old.resets_at);
                        }
                        None => entry.limits.push(limit.clone()),
                    }
                }
                entry.limits.sort_by_key(|limit| limit.window_minutes.unwrap_or(u64::MAX));
                entry.updated_at = Some(Utc::now());
                if entry.source != Some(LimitsSource::Live) {
                    entry.source = Some(LimitsSource::Session);
                }
            }
            self.publish();
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UsageLimitsResult> {
        self.inner.changed.subscribe()
    }

    /// Answer `usage.limits`.
    pub async fn limits(&self, params: UsageLimitsParams) -> UsageLimitsResult {
        self.sync_accounts();
        self.seed();
        let due = {
            let mut state = self.lock();
            state.last_interest = Some(Instant::now());
            let due = LIVE
                .into_iter()
                .filter(|kind| {
                    let attempted = state.providers.get(kind).and_then(|entry| entry.attempted);
                    let spacing = if params.refresh { MIN_SPACING } else { fresh_for(*kind) };
                    attempted.is_none_or(|at| at.elapsed() >= spacing)
                })
                // A background read already in flight answers a cached call.
                .filter(|kind| !(params.cached && state.refreshing.contains(kind)))
                .collect::<Vec<_>>();
            if params.cached {
                // Mark before spawning so the snapshot below reports them.
                state.refreshing.extend(due.iter().copied());
            }
            due
        };
        let account_due = self.due_accounts(&params);
        if params.cached {
            for kind in due {
                let monitor = self.clone();
                tokio::spawn(async move { monitor.refresh(kind, MIN_SPACING).await });
            }
            for key in account_due {
                let monitor = self.clone();
                tokio::spawn(async move { monitor.refresh_account(key, MIN_SPACING).await });
            }
        } else {
            let reads = due.into_iter().map(|kind| self.refresh(kind, MIN_SPACING));
            let account_reads = account_due.into_iter().map(|key| self.refresh_account(key, MIN_SPACING));
            let _ = tokio::time::timeout(WAIT, async {
                futures::future::join(futures::future::join_all(reads), futures::future::join_all(account_reads)).await
            })
            .await;
        }
        self.snapshot()
    }

    /// The named accounts (and the CLI account, when another account is the
    /// default) a request asks about and whose cache is older than the
    /// freshness window. The default account's own limits are the global
    /// entry, so it is never read twice.
    fn due_accounts(&self, params: &UsageLimitsParams) -> Vec<AccountKey> {
        if params.instances.is_empty() && !params.all_accounts {
            return Vec::new();
        }
        let settings = self.inner.settings.get();
        let mut wanted: Vec<AccountKey> = Vec::new();
        for instance in &params.instances {
            wanted.push((instance.kind, instance.instance.clone()));
        }
        if params.all_accounts {
            for kind in LIVE {
                let raw = settings.providers.get(&kind).cloned().unwrap_or_default();
                if raw.accounts.is_empty() {
                    continue;
                }
                wanted.push((kind, "default".into()));
                wanted.extend(raw.accounts.keys().map(|id| (kind, id.clone())));
            }
        }
        let mut state = self.lock();
        let mut due = Vec::new();
        for key in wanted {
            let raw = settings.providers.get(&key.0).cloned().unwrap_or_default();
            let known = key.1 == "default" || raw.accounts.contains_key(&key.1);
            if !LIVE.contains(&key.0) || !known || key.1 == crate::provider_accounts::resolve(&raw, None, None) || due.contains(&key) {
                continue;
            }
            let attempted = state.accounts.get(&key).and_then(|entry| entry.attempted);
            let spacing = if params.refresh { MIN_SPACING } else { fresh_for(key.0) };
            if attempted.is_some_and(|at| at.elapsed() < spacing) || (params.cached && state.accounts_refreshing.contains(&key)) {
                continue;
            }
            if params.cached {
                state.accounts_refreshing.insert(key.clone());
            }
            due.push(key);
        }
        due
    }

    /// Forget one account's cached limits (signed out, removed, or signed in again).
    pub fn forget_account(&self, kind: ProviderKind, instance: &str) {
        self.seed();
        self.lock().accounts.remove(&(kind, instance.to_string()));
        self.publish();
    }

    async fn refresh_account(&self, key: AccountKey, spacing: Duration) {
        let gate = self.inner.account_gates.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(key.clone()).or_default().clone();
        let _gate = gate.lock().await;
        let recent = self.lock().accounts.get(&key).and_then(|entry| entry.attempted).is_some_and(|at| at.elapsed() < spacing);
        if !recent {
            self.lock().accounts_refreshing.insert(key.clone());
            let read = self.read_account(&key.0, &key.1).await;
            let mut state = self.lock();
            let entry = state.accounts.entry(key.clone()).or_default();
            entry.attempted = Some(Instant::now());
            match read {
                Ok((limits, plan)) => {
                    entry.limits = limits;
                    entry.plan = plan.or(entry.plan.take());
                    entry.updated_at = Some(Utc::now());
                    entry.source = Some(LimitsSource::Live);
                    entry.stale = None;
                }
                Err(unread) => {
                    tracing::debug!(provider = %key.0, reason = ?unread.reason, "could not read a named account's limits");
                    entry.stale = Some((unread.reason, unread.retry_at));
                }
            }
        }
        self.lock().accounts_refreshing.remove(&key);
        self.publish();
    }

    async fn read_account(&self, kind: &ProviderKind, instance: &str) -> Result<(Vec<UsageLimit>, Option<String>), UsageUnread> {
        let bound = self.inner.bound.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(bound) = bound
            && let Some(read) = bound.read(*kind, instance).await
        {
            return read;
        }
        let unavailable = |detail: &str| UsageUnread { reason: LimitsStale::Unavailable, retry_at: None, detail: detail.into() };
        let settings = self.inner.settings.get();
        let provider = settings.providers.get(kind).cloned().unwrap_or_default();
        let env = crate::provider_accounts::environment(&provider, *kind, instance).map_err(|error| unavailable(&error.to_string()))?;
        let cwd = self.inner.settings.dir().to_path_buf();
        self.read_with(*kind, provider.binary.map(Into::into), &cwd, &env).await
    }

    /// Fold limits a running turn reported. Values from a live read and from
    /// a turn describe the same windows; the newer one wins per window.
    pub fn observe(&self, kind: ProviderKind, limits: &[UsageLimit]) {
        if limits.is_empty() {
            return;
        }
        self.sync_accounts();
        self.seed();
        {
            let mut state = self.lock();
            let entry = state.providers.entry(kind).or_default();
            for limit in limits {
                let key = window_key(limit);
                match entry.limits.iter_mut().find(|old| window_key(old) == key) {
                    Some(old) => {
                        old.used_percent = limit.used_percent;
                        old.resets_at = limit.resets_at.or(old.resets_at);
                    }
                    None => entry.limits.push(limit.clone()),
                }
            }
            entry.limits.sort_by_key(|limit| limit.window_minutes.unwrap_or(u64::MAX));
            entry.updated_at = Some(Utc::now());
            if entry.source != Some(LimitsSource::Live) {
                entry.source = Some(LimitsSource::Session);
            }
            // A running turn means the provider renewed its login; the next read can run.
            if entry.stale.is_some_and(|(reason, _)| reason == LimitsStale::LoginRefresh) {
                entry.stale = None;
            }
        }
        self.publish();
    }

    /// A turn ended on `kind`. Re-read its limits once the provider settles,
    /// if a client is watching. Several turns ending together share one read.
    pub fn turn_finished(&self, kind: ProviderKind) {
        if !LIVE.contains(&kind) {
            return;
        }
        {
            let mut state = self.lock();
            if state.last_interest.is_none_or(|at| at.elapsed() > INTEREST) || !state.after_turn.insert(kind) {
                return;
            }
        }
        let monitor = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(settle_after_turn(kind)).await;
            monitor.lock().after_turn.remove(&kind);
            monitor.lock().refreshing.insert(kind);
            monitor.refresh(kind, MIN_SPACING).await;
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Load the last stored report once, so the first answer after a restart
    /// is not empty while live reads run.
    fn seed(&self) {
        if self.lock().seeded {
            return;
        }
        let stored = self.inner.store.latest_provider_limits().unwrap_or_else(|error| {
            tracing::warn!(%error, "could not read stored usage limits");
            Vec::new()
        });
        let saved = self.read_saved();
        let identities: BTreeMap<ProviderKind, String> = LIVE.into_iter().map(|kind| (kind, self.identity(kind))).collect();
        let mut state = self.lock();
        if state.seeded {
            return;
        }
        state.seeded = true;
        let settings = self.inner.settings.get();
        // The last run's values come first: they are newer than the event log's.
        // They describe the account they were read for, so a changed account,
        // binary or environment leaves them out.
        for provider in saved.providers {
            if identities.get(&provider.provider) != Some(&provider.identity) {
                continue;
            }
            let entry = state.providers.entry(provider.provider).or_default();
            if entry.updated_at.is_none() {
                provider.entry.load_into(entry);
            }
        }
        for account in saved.accounts {
            let known = settings.providers.get(&account.provider).is_some_and(|raw| raw.accounts.contains_key(&account.instance));
            if !LIVE.contains(&account.provider) || !(known || account.instance == "default") {
                continue;
            }
            let entry = state.accounts.entry((account.provider, account.instance)).or_default();
            if entry.updated_at.is_none() {
                account.entry.load_into(entry);
            }
        }
        for report in stored {
            // Legacy reports do not contain account identity. Once accounts
            // are configured, a live read is required instead of guessing.
            if settings
                .providers
                .get(&report.provider)
                .is_some_and(|provider| !provider.accounts.is_empty() || provider.default_account.is_some())
            {
                continue;
            }
            let entry = state.providers.entry(report.provider).or_default();
            if entry.updated_at.is_none() {
                entry.limits = report.limits;
                entry.updated_at = report.updated_at;
                entry.source = Some(LimitsSource::Stored);
            }
        }
    }

    /// Read one provider's limits now unless another read finished within
    /// `spacing`. Always publishes, so clients clear their refreshing state.
    async fn refresh(&self, kind: ProviderKind, spacing: Duration) {
        let Some(gate) = self.inner.gates.get(&kind) else { return };
        let _gate = gate.lock().await;
        self.sync_accounts();
        let identity = self.identity(kind);
        let recent = self.lock().providers.get(&kind).and_then(|entry| entry.attempted).is_some_and(|at| at.elapsed() < spacing);
        if !recent {
            self.lock().refreshing.insert(kind);
            let read = self.read(kind).await;
            self.sync_accounts();
            if self.identity(kind) != identity {
                self.lock().refreshing.remove(&kind);
                self.publish();
                return;
            }
            let mut state = self.lock();
            let entry = state.providers.entry(kind).or_default();
            entry.attempted = Some(Instant::now());
            apply_read(entry, kind, read);
        }
        self.lock().refreshing.remove(&kind);
        self.publish();
    }

    async fn read(&self, kind: ProviderKind) -> Result<(Vec<UsageLimit>, Option<String>), UsageUnread> {
        let settings = self.inner.settings.get();
        let provider = crate::settings::provider_settings(&settings, kind, None);
        let binary: Option<PathBuf> = provider.binary.clone().map(Into::into);
        // cwd only needs to be a real directory; account auth lives under $HOME.
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        self.read_with(kind, binary, &home, &provider.env).await
    }

    async fn read_with(
        &self,
        kind: ProviderKind,
        binary: Option<PathBuf>,
        home: &std::path::Path,
        env: &BTreeMap<String, String>,
    ) -> Result<(Vec<UsageLimit>, Option<String>), UsageUnread> {
        let started = Instant::now();
        let unavailable = |detail: &str| UsageUnread { reason: LimitsStale::Unavailable, retry_at: None, detail: detail.into() };
        let read = match kind {
            ProviderKind::ClaudeCode => {
                tokio::time::timeout(Duration::from_secs(20), kybern_drivers::claude::read_account_usage(home, binary.as_ref(), env))
                    .await
                    .unwrap_or_else(|_| Err(unavailable("the read timed out")))
            }
            ProviderKind::Codex => {
                tokio::time::timeout(Duration::from_secs(8), kybern_drivers::codex::read_account_limits(home, binary.as_ref(), env))
                    .await
                    .ok()
                    .flatten()
                    .map(|limits| (limits, None))
                    .ok_or_else(|| unavailable("Codex reported no rate limits"))
            }
            ProviderKind::Cursor => tokio::time::timeout(Duration::from_secs(10), kybern_drivers::cursor::usage::read_account_usage(env))
                .await
                .ok()
                .flatten()
                .map(|usage| (usage.limits, usage.plan))
                .ok_or_else(|| unavailable("Cursor reported no usage")),
            // Read with the token `agy` already refreshed; an expired one reads as stale (`LoginRefresh`).
            ProviderKind::Antigravity => {
                tokio::time::timeout(Duration::from_secs(12), kybern_drivers::antigravity::read_account_usage(home, binary.as_ref(), env))
                    .await
                    .unwrap_or_else(|_| Err(unavailable("the read timed out")))
            }
            _ => Err(unavailable("no account limits to read")),
        };
        tracing::debug!(provider = %kind, ok = read.is_ok(), elapsed_ms = started.elapsed().as_millis() as u64, "read account limits");
        read.and_then(|(limits, plan)| if limits.is_empty() { Err(unavailable("the read returned no limits")) } else { Ok((limits, plan)) })
    }

    fn publish(&self) {
        // No receivers just means no client is connected.
        let _ = self.inner.changed.send(self.snapshot());
        self.schedule_persist();
    }

    // ---- the cache file ----

    fn read_saved(&self) -> SavedLimits {
        match std::fs::read(&self.inner.cache_file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring an unreadable usage limits cache");
                SavedLimits::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SavedLimits::default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the usage limits cache");
                SavedLimits::default()
            }
        }
    }

    /// What to keep for the next run: values a live read or a turn reported.
    /// The event log's older report is not copied.
    fn saved_limits(&self) -> SavedLimits {
        let state = self.lock();
        SavedLimits {
            version: 1,
            providers: state
                .providers
                .iter()
                .filter_map(|(kind, entry)| {
                    Some(SavedProvider { provider: *kind, identity: self.identity(*kind), entry: SavedEntry::from_entry(entry)? })
                })
                .collect(),
            accounts: state
                .accounts
                .iter()
                .filter_map(|((kind, instance), entry)| {
                    Some(SavedAccount { provider: *kind, instance: instance.clone(), entry: SavedEntry::from_entry(entry)? })
                })
                .collect(),
        }
    }

    /// Write the cache file now, unless nothing changed since the last write.
    /// Nothing is written before the file was read, or its content would be lost.
    fn persist_now(&self) {
        if !self.lock().seeded {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&self.saved_limits()) else { return };
        let mut last = self.inner.last_written.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.as_deref() == Some(bytes.as_slice()) {
            return;
        }
        match write_atomic(&self.inner.cache_file, &bytes) {
            Ok(()) => *last = Some(bytes),
            Err(error) => tracing::warn!(%error, file = %self.inner.cache_file.display(), "could not save the usage limits cache"),
        }
    }

    /// Save soon, and once for a burst of changes.
    fn schedule_persist(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.persist_now();
            return;
        };
        {
            let mut state = self.lock();
            if state.persist_scheduled {
                return;
            }
            state.persist_scheduled = true;
        }
        let monitor = self.clone();
        let delay = *self.inner.persist_delay.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            monitor.lock().persist_scheduled = false;
            let writer = monitor.clone();
            let _ = tokio::task::spawn_blocking(move || writer.persist_now()).await;
        });
    }

    #[cfg(test)]
    fn set_persist_delay(&self, delay: Duration) {
        *self.inner.persist_delay.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = delay;
    }

    /// The cache as clients see it. A window whose reset time has passed
    /// reads as unused until the next report says otherwise; it keeps the
    /// passed reset time, so clients can tell it was not read since.
    fn snapshot(&self) -> UsageLimitsResult {
        let state = self.lock();
        let now = Utc::now().timestamp();
        let mut providers = state
            .providers
            .iter()
            .filter(|(_, entry)| !entry.limits.is_empty())
            .map(|(kind, entry)| ProviderLimits {
                provider: *kind,
                limits: entry
                    .limits
                    .iter()
                    .map(|limit| match limit.resets_at {
                        Some(reset) if reset <= now => UsageLimit { used_percent: 0.0, ..limit.clone() },
                        _ => limit.clone(),
                    })
                    .collect(),
                updated_at: entry.updated_at,
                source: entry.source,
                plan: entry.plan.clone(),
                stale: entry.stale.map(|(reason, _)| reason),
                retry_at: entry.stale.and_then(|(_, retry_at)| retry_at),
                instance: None,
            })
            .collect::<Vec<_>>();
        providers.sort_by_key(|entry| provider_order(entry.provider));
        let settings = self.inner.settings.get();
        let mut accounts = Vec::new();
        for ((kind, instance), entry) in state.accounts.iter().filter(|(_, entry)| !entry.limits.is_empty()) {
            // The current default is reported from the global entry below; an
            // entry read while it was not the default would duplicate it.
            let default = settings.providers.get(kind).map(|raw| crate::provider_accounts::resolve(raw, None, None));
            let global = state.providers.get(kind).is_some_and(|entry| !entry.limits.is_empty());
            if global && default.as_deref().unwrap_or("default") == instance {
                continue;
            }
            accounts.push(entry_limits(*kind, Some(instance.clone()), entry, now));
        }
        // The default account's limits are the global entry; label it so clients
        // find every account in one list.
        for kind in LIVE {
            if !state.accounts.keys().any(|(k, _)| *k == kind) && settings.providers.get(&kind).is_none_or(|p| p.accounts.is_empty()) {
                continue;
            }
            if let Some(entry) = state.providers.get(&kind).filter(|entry| !entry.limits.is_empty()) {
                let raw = settings.providers.get(&kind).cloned().unwrap_or_default();
                accounts.push(entry_limits(kind, Some(crate::provider_accounts::resolve(&raw, None, None)), entry, now));
            }
        }
        accounts.sort_by_key(|entry| (provider_order(entry.provider), entry.instance.clone()));
        let mut refreshing = state.refreshing.iter().copied().collect::<Vec<_>>();
        refreshing.sort_by_key(|kind| provider_order(*kind));
        UsageLimitsResult { providers, refreshing, accounts }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn monitor() -> UsageMonitor {
        let root = std::env::temp_dir().join(format!("kybern-usage-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let settings = SettingsStore::load(&root.join("settings.json")).unwrap();
        UsageMonitor::new(Store::open_in_memory().unwrap(), settings)
    }

    fn limit(name: &str, used_percent: f64, window_minutes: Option<u64>, resets_at: Option<i64>) -> UsageLimit {
        UsageLimit { name: name.into(), used_percent, window_minutes, resets_at }
    }

    #[tokio::test]
    async fn session_reports_merge_by_window_and_publish() {
        let monitor = monitor();
        let mut changes = monitor.subscribe();
        let future = Utc::now().timestamp() + 3600;
        monitor.observe(ProviderKind::ClaudeCode, &[limit("Current session", 40.0, Some(300), Some(future))]);
        monitor.observe(ProviderKind::ClaudeCode, &[limit("5-hour", 55.0, Some(300), None)]);
        let mut latest = changes.recv().await.unwrap();
        while let Ok(next) = changes.try_recv() {
            latest = next;
        }
        let claude = &latest.providers[0];
        assert_eq!(claude.provider, ProviderKind::ClaudeCode);
        assert_eq!(claude.limits.len(), 1, "one row per window");
        assert_eq!(claude.limits[0].used_percent, 55.0);
        assert_eq!(claude.limits[0].resets_at, Some(future), "a report without a reset keeps the known one");
        assert_eq!(claude.source, Some(LimitsSource::Session));
        assert!(claude.updated_at.is_some());
    }

    #[tokio::test]
    async fn passed_resets_read_as_unused() {
        let monitor = monitor();
        let past = Utc::now().timestamp() - 60;
        monitor.observe(ProviderKind::Codex, &[limit("Primary", 97.0, Some(300), Some(past))]);
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.providers[0].limits[0].used_percent, 0.0);
        assert_eq!(snapshot.providers[0].limits[0].resets_at, Some(past), "clients see that it reset since the reading");
    }

    #[tokio::test]
    async fn clients_see_why_values_are_old_until_a_turn_renews_the_login() {
        let monitor = monitor();
        let claude = ProviderKind::ClaudeCode;
        monitor.observe(claude, &[limit("Current session", 10.0, Some(300), None)]);
        monitor.lock().providers.get_mut(&claude).unwrap().stale = Some((LimitsStale::LoginRefresh, None));
        assert_eq!(monitor.snapshot().providers[0].stale, Some(LimitsStale::LoginRefresh));
        monitor.observe(claude, &[limit("Current session", 20.0, Some(300), None)]);
        assert_eq!(monitor.snapshot().providers[0].stale, None, "a running turn renewed the login");

        let retry_at = Utc::now() + chrono::Duration::minutes(5);
        monitor.lock().providers.get_mut(&claude).unwrap().stale = Some((LimitsStale::Throttled, Some(retry_at)));
        monitor.observe(claude, &[limit("Current session", 30.0, Some(300), None)]);
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.providers[0].stale, Some(LimitsStale::Throttled), "a turn does not lift throttling");
        assert_eq!(snapshot.providers[0].retry_at, Some(retry_at));
    }

    #[tokio::test]
    async fn turns_refresh_only_while_a_client_is_watching() {
        let monitor = monitor();
        monitor.turn_finished(ProviderKind::Codex);
        assert!(monitor.lock().after_turn.is_empty(), "no client asked yet");
        monitor.lock().last_interest = Some(Instant::now());
        monitor.turn_finished(ProviderKind::Codex);
        monitor.turn_finished(ProviderKind::Codex);
        assert_eq!(monitor.lock().after_turn.len(), 1, "turns ending together share a read");
        monitor.turn_finished(ProviderKind::Opencode);
        assert!(!monitor.lock().after_turn.contains(&ProviderKind::Opencode), "no account limits to read");
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;
    #[test]
    fn global_account_changes_clear_cached_limits_and_isolate_session_reports() {
        let monitor = super::tests::monitor();
        monitor.observe(
            ProviderKind::Codex,
            &[UsageLimit { name: "5-hour".into(), used_percent: 99.0, window_minutes: Some(300), resets_at: None }],
        );
        let mut settings = monitor.inner.settings.get();
        let provider = settings.providers.entry(ProviderKind::Codex).or_default();
        provider.accounts.insert(
            "work".into(),
            kybern_protocol::ProviderAccount {
                name: "Work".into(),
                directory: std::env::temp_dir().join("account-work").to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        provider.default_account = Some("work".into());
        monitor.inner.settings.set(settings).unwrap();
        monitor.settings_changed();
        assert!(monitor.snapshot().providers.is_empty());
        monitor.observe_account(
            &ProviderInstance::default_for(ProviderKind::Codex),
            &[UsageLimit { name: "5-hour".into(), used_percent: 99.0, window_minutes: Some(300), resets_at: None }],
        );
        assert!(monitor.snapshot().providers.is_empty());
        monitor.observe_account(
            &ProviderInstance { kind: ProviderKind::Codex, instance: "work".into() },
            &[UsageLimit { name: "5-hour".into(), used_percent: 10.0, window_minutes: Some(300), resets_at: None }],
        );
        assert_eq!(monitor.snapshot().providers[0].limits[0].used_percent, 10.0);
    }
}

#[cfg(test)]
mod named_account_tests {
    use super::*;

    fn with_accounts() -> UsageMonitor {
        let monitor = super::tests::monitor();
        let mut settings = monitor.inner.settings.get();
        let provider = settings.providers.entry(ProviderKind::ClaudeCode).or_default();
        provider.accounts.insert(
            "work".into(),
            kybern_protocol::ProviderAccount {
                name: "Work".into(),
                directory: std::env::temp_dir().join("account-work").to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        monitor.inner.settings.set(settings).unwrap();
        monitor
    }

    #[test]
    fn a_plain_poll_reads_no_named_accounts() {
        let monitor = with_accounts();
        assert!(monitor.due_accounts(&UsageLimitsParams { cached: true, ..Default::default() }).is_empty());
    }

    #[test]
    fn interest_in_accounts_reads_only_those_and_skips_the_default_account() {
        let monitor = with_accounts();
        let due = monitor.due_accounts(&UsageLimitsParams { cached: true, all_accounts: true, ..Default::default() });
        assert!(due.contains(&(ProviderKind::ClaudeCode, "work".into())));
        assert!(!due.contains(&(ProviderKind::ClaudeCode, "default".into())), "the default account is the global entry");
        let again = monitor.due_accounts(&UsageLimitsParams { cached: true, all_accounts: true, ..Default::default() });
        assert!(again.is_empty(), "a read already in flight answers the second call");
        let instances = monitor.due_accounts(&UsageLimitsParams {
            instances: vec![ProviderInstance { kind: ProviderKind::ClaudeCode, instance: "missing".into() }],
            ..Default::default()
        });
        assert!(instances.is_empty(), "unknown accounts are never read");
    }

    #[test]
    fn an_account_that_became_the_default_is_reported_once() {
        let monitor = with_accounts();
        let limit = UsageLimit { name: "5-hour".into(), used_percent: 30.0, window_minutes: Some(300), resets_at: None };
        let work = ProviderInstance { kind: ProviderKind::ClaudeCode, instance: "work".into() };
        monitor.observe_account(&work, std::slice::from_ref(&limit));
        let mut settings = monitor.inner.settings.get();
        settings.providers.entry(ProviderKind::ClaudeCode).or_default().default_account = Some("work".into());
        monitor.inner.settings.set(settings).unwrap();
        monitor.observe(ProviderKind::ClaudeCode, std::slice::from_ref(&limit));
        let snapshot = monitor.snapshot();
        let labeled = snapshot.accounts.iter().filter(|entry| entry.instance.as_deref() == Some("work")).count();
        assert_eq!(labeled, 1, "{:?}", snapshot.accounts);
    }

    #[test]
    fn switching_the_default_swaps_limits_instead_of_dropping_them() {
        let monitor = with_accounts();
        monitor.settings_changed();
        let five = |used: f64| UsageLimit { name: "5-hour".into(), used_percent: used, window_minutes: Some(300), resets_at: None };
        monitor.observe(ProviderKind::ClaudeCode, &[five(70.0)]);
        monitor.observe_account(&ProviderInstance { kind: ProviderKind::ClaudeCode, instance: "work".into() }, &[five(20.0)]);
        let mut settings = monitor.inner.settings.get();
        settings.providers.entry(ProviderKind::ClaudeCode).or_default().default_account = Some("work".into());
        monitor.inner.settings.set(settings).unwrap();
        monitor.settings_changed();
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.providers[0].limits[0].used_percent, 20.0, "the new default starts from its own limits");
        let cli = snapshot.accounts.iter().find(|entry| entry.instance.as_deref() == Some("default")).unwrap();
        assert_eq!(cli.limits[0].used_percent, 70.0, "the CLI account keeps its limits");
        assert_eq!(snapshot.accounts.iter().filter(|entry| entry.instance.as_deref() == Some("work")).count(), 1);
    }

    #[test]
    fn named_account_reports_appear_labeled_and_forget_clears_them() {
        let monitor = with_accounts();
        let limit = UsageLimit { name: "5-hour".into(), used_percent: 30.0, window_minutes: Some(300), resets_at: None };
        monitor
            .observe_account(&ProviderInstance { kind: ProviderKind::ClaudeCode, instance: "work".into() }, std::slice::from_ref(&limit));
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.accounts.len(), 1);
        assert_eq!(snapshot.accounts[0].instance.as_deref(), Some("work"));
        assert!(snapshot.providers.is_empty(), "the global default is untouched");
        monitor.forget_account(ProviderKind::ClaudeCode, "work");
        assert!(monitor.snapshot().accounts.is_empty());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("kybern-usage-persist-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A monitor over the data directory `root`, as a daemon start would build.
    fn monitor_at(root: &std::path::Path) -> UsageMonitor {
        let settings = SettingsStore::load(&root.join("settings.json")).unwrap();
        UsageMonitor::new(Store::open_in_memory().unwrap(), settings)
    }

    fn file(root: &std::path::Path) -> PathBuf {
        root.join("cache").join("usage-limits.json")
    }

    fn limit(name: &str, used_percent: f64, window_minutes: u64, resets_at: Option<i64>) -> UsageLimit {
        UsageLimit { name: name.into(), used_percent, window_minutes: Some(window_minutes), resets_at }
    }

    /// Start a monitor the way a client's first request would, without any live read.
    fn started(root: &std::path::Path) -> UsageMonitor {
        let monitor = monitor_at(root);
        monitor.sync_accounts();
        monitor.seed();
        monitor
    }

    fn work_account() -> kybern_protocol::ProviderAccount {
        kybern_protocol::ProviderAccount {
            name: "Work".into(),
            directory: std::env::temp_dir().join("account-work").to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn limits_survive_a_restart_and_are_marked_as_from_the_last_run() {
        let root = root();
        let future = Utc::now().timestamp() + 3600;
        let before = started(&root);
        before.observe(ProviderKind::ClaudeCode, &[limit("Current session", 40.0, 300, Some(future)), limit("Weekly", 12.0, 10080, None)]);
        let saved_at = before.snapshot().providers[0].updated_at.expect("a report has a time");
        before.persist_now();
        assert!(file(&root).exists());
        assert_eq!(std::fs::read_dir(root.join("cache")).unwrap().count(), 1, "no temporary file is left behind");

        let after = started(&root);
        let snapshot = after.snapshot();
        let claude = &snapshot.providers[0];
        assert_eq!(claude.provider, ProviderKind::ClaudeCode);
        assert_eq!(claude.limits.len(), 2, "every window comes back");
        assert_eq!((claude.limits[0].used_percent, claude.limits[0].resets_at), (40.0, Some(future)));
        assert_eq!(claude.updated_at, Some(saved_at), "the original time, not the restart's");
        assert_eq!(claude.source, Some(LimitsSource::Session));
        assert_eq!(claude.stale, Some(LimitsStale::FromLastRun));
        let wire = serde_json::to_value(claude).unwrap();
        assert_eq!(wire["stale"], serde_json::json!("from_last_run"));
    }

    #[test]
    fn a_live_read_replaces_the_last_runs_values_and_a_failed_one_keeps_them_with_its_reason() {
        let root = root();
        let before = started(&root);
        before.observe(ProviderKind::Codex, &[limit("5-hour", 70.0, 300, None)]);
        before.persist_now();

        let after = started(&root);
        let codex = ProviderKind::Codex;
        // The login is due for a refresh: the values stay, with that reason instead.
        {
            let mut state = after.lock();
            let unread = UsageUnread { reason: LimitsStale::LoginRefresh, retry_at: None, detail: "login due".into() };
            apply_read(state.providers.get_mut(&codex).unwrap(), codex, Err(unread));
        }
        let kept = after.snapshot();
        assert_eq!(kept.providers[0].limits[0].used_percent, 70.0, "a provider with cached values is never hidden");
        assert_eq!(kept.providers[0].stale, Some(LimitsStale::LoginRefresh));
        // Any other failed read says so.
        {
            let mut state = after.lock();
            let unread = UsageUnread { reason: LimitsStale::Unavailable, retry_at: None, detail: "offline".into() };
            apply_read(state.providers.get_mut(&codex).unwrap(), codex, Err(unread));
        }
        assert_eq!(after.snapshot().providers[0].stale, Some(LimitsStale::Unavailable));
        // A live read makes them current.
        {
            let mut state = after.lock();
            apply_read(state.providers.get_mut(&codex).unwrap(), codex, Ok((vec![limit("5-hour", 15.0, 300, None)], Some("Pro".into()))));
        }
        let fresh = after.snapshot();
        assert_eq!((fresh.providers[0].limits[0].used_percent, fresh.providers[0].stale), (15.0, None));
        assert_eq!((fresh.providers[0].source, fresh.providers[0].plan.as_deref()), (Some(LimitsSource::Live), Some("Pro")));
        // The new values are what the next run starts from.
        after.persist_now();
        let next = started(&root);
        assert_eq!(next.snapshot().providers[0].limits[0].used_percent, 15.0);
        assert_eq!(next.snapshot().providers[0].plan.as_deref(), Some("Pro"));
    }

    #[test]
    fn values_of_another_account_are_not_restored() {
        let root = root();
        let before = started(&root);
        before.observe(ProviderKind::Codex, &[limit("5-hour", 90.0, 300, None)]);
        before.persist_now();

        // The user switched the default account since: those values are someone else's.
        let settings = SettingsStore::load(&root.join("settings.json")).unwrap();
        let mut changed = settings.get();
        let provider = changed.providers.entry(ProviderKind::Codex).or_default();
        provider.accounts.insert("work".into(), work_account());
        provider.default_account = Some("work".into());
        settings.set(changed).unwrap();
        let after = UsageMonitor::new(Store::open_in_memory().unwrap(), settings);
        after.sync_accounts();
        after.seed();
        assert!(after.snapshot().providers.is_empty());
    }

    #[test]
    fn named_accounts_are_restored_while_they_still_exist() {
        let root = root();
        let settings = SettingsStore::load(&root.join("settings.json")).unwrap();
        let mut with_work = settings.get();
        with_work.providers.entry(ProviderKind::ClaudeCode).or_default().accounts.insert("work".into(), work_account());
        settings.set(with_work).unwrap();
        let build = || {
            let monitor = UsageMonitor::new(Store::open_in_memory().unwrap(), settings.clone());
            monitor.sync_accounts();
            monitor.seed();
            monitor
        };
        let before = build();
        before.observe_account(
            &ProviderInstance { kind: ProviderKind::ClaudeCode, instance: "work".into() },
            &[limit("5-hour", 33.0, 300, None)],
        );
        before.persist_now();

        let after = build();
        let accounts = after.snapshot().accounts;
        assert_eq!(accounts.len(), 1);
        assert_eq!((accounts[0].instance.as_deref(), accounts[0].limits[0].used_percent), (Some("work"), 33.0));
        assert_eq!(accounts[0].stale, Some(LimitsStale::FromLastRun));

        // Forgetting an account removes it from the file too.
        after.forget_account(ProviderKind::ClaudeCode, "work");
        after.persist_now();
        assert!(build().snapshot().accounts.is_empty());
    }

    #[test]
    fn nothing_is_written_before_the_file_was_read_and_a_damaged_file_is_ignored() {
        let root = root();
        let before = started(&root);
        before.observe(ProviderKind::Cursor, &[limit("Monthly", 50.0, 43200, None)]);
        before.persist_now();
        let saved = std::fs::read(file(&root)).unwrap();

        // A monitor that has not loaded the file cannot overwrite it with nothing.
        let fresh = monitor_at(&root);
        fresh.publish();
        fresh.persist_now();
        assert_eq!(std::fs::read(file(&root)).unwrap(), saved);

        std::fs::write(file(&root), "{ not json").unwrap();
        let damaged = started(&root);
        assert!(damaged.snapshot().providers.is_empty());
        damaged.observe(ProviderKind::Cursor, &[limit("Monthly", 51.0, 43200, None)]);
        damaged.persist_now();
        assert_eq!(started(&root).snapshot().providers[0].limits[0].used_percent, 51.0, "the damaged file was replaced");
    }

    #[test]
    fn the_event_logs_older_report_is_not_copied_into_the_file() {
        let root = root();
        let monitor = started(&root);
        {
            let mut state = monitor.lock();
            let entry = state.providers.entry(ProviderKind::Codex).or_default();
            entry.limits = vec![limit("5-hour", 5.0, 300, None)];
            entry.updated_at = Some(Utc::now());
            entry.source = Some(LimitsSource::Stored);
        }
        monitor.persist_now();
        let saved: SavedLimits = serde_json::from_slice(&std::fs::read(file(&root)).unwrap()).unwrap();
        assert!(saved.providers.is_empty() && saved.accounts.is_empty());
    }

    #[tokio::test]
    async fn changes_are_written_once_after_a_short_delay() {
        let root = root();
        let monitor = started(&root);
        monitor.set_persist_delay(Duration::from_millis(30));
        monitor.observe(ProviderKind::ClaudeCode, &[limit("Current session", 10.0, 300, None)]);
        monitor.observe(ProviderKind::ClaudeCode, &[limit("Current session", 20.0, 300, None)]);
        assert!(!file(&root).exists(), "the write waits for its delay");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !file(&root).exists() {
            assert!(Instant::now() < deadline, "the cache file was never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let saved: SavedLimits = serde_json::from_slice(&std::fs::read(file(&root)).unwrap()).unwrap();
        assert_eq!(saved.providers.len(), 1);
        assert_eq!(saved.providers[0].entry.limits[0].used_percent, 20.0, "the burst was written once, with its last values");
    }
}
