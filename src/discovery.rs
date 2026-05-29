//! Bonjour / mDNS-SD service registration + discovery.
//!
//! Why this exists: when a peer machine has multiple interfaces on
//! the same subnet (Mac with Wi-Fi + Ethernet, Linux laptop with
//! Wi-Fi + USB-C dock, etc.), plain hostname resolution returns
//! every interface's IP and the dialer has no way to tell which one
//! the OS would *prefer* for outbound traffic. The current connect
//! path races them and uses whichever DTLS handshake completes first,
//! which is RTT-roughly-correct but not always what the user wanted
//! — Wi-Fi can win the race even when the user has Ethernet ranked
//! higher in macOS's service order.
//!
//! Each lan-mouse instance registers a `_lan-mouse._udp.local.`
//! Bonjour service whose TXT record advertises `primary=<ip>`, where
//! `<ip>` is the IPv4 of the interface that owns the default route
//! (which on macOS reflects service order), and `fp=<sha256>` for the
//! local DTLS certificate fingerprint. The dialer browses the same
//! service type, looks up the peer instance by hostname, and prepends
//! the primary IP to its connection-attempt list. If the peer is on an
//! old version with no advertised service (or mDNS is firewalled),
//! nothing breaks — we silently fall through to the existing
//! `connect_any` race.
//!
//! The whole subsystem is gated by the `mdns_discovery` config flag
//! (default true). Toggling it off shuts down the mDNS daemon and
//! all browse/registration state — useful on networks where mDNS
//! multicast (224.0.0.251) is firewalled.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs, io,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::task::{JoinHandle, spawn_local};

use crate::crypto::normalize_fingerprint;

pub(crate) const SERVICE_TYPE: &str = "_lan-mouse._udp.local.";
pub(crate) const TXT_PRIMARY_KEY: &str = "primary";
pub(crate) const TXT_FINGERPRINT_KEY: &str = "fp";
const LAST_SUCCESS_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Cross-platform: IP of the interface that owns the default route.
///
/// On macOS the default route reflects the user's service-order
/// ranking — that's exactly the "primary" the user expects when they
/// say "use Ethernet, not Wi-Fi". On Linux it reflects the lowest-
/// metric default route. On Windows it's whatever
/// `GetBestRoute2` selects.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let iface = netdev::get_default_interface().ok()?;
    iface.ipv4.first().map(|net| net.addr())
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "lan-mouse".to_string())
}

/// Strip a single trailing dot if present. Bonjour hostnames are
/// stored as fully-qualified ("foo.local."); user config typically
/// writes them without the trailing dot ("foo.local"). Normalize
/// to compare.
pub(crate) fn strip_trailing_dot(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

fn strip_bonjour_collision_suffix(s: &str) -> &str {
    let Some((base, suffix)) = s.rsplit_once(" (") else {
        return s;
    };
    let Some(number) = suffix.strip_suffix(')') else {
        return s;
    };
    if number.chars().all(|c| c.is_ascii_digit()) {
        base
    } else {
        s
    }
}

/// Pull the service-instance label off a Bonjour fullname.
///
/// `mdns-sd` returns fullnames as `"<instance>.<service-type>"` where
/// `<service-type>` is e.g. `"_lan-mouse._udp.local."`. The instance
/// label is the user-visible identifier the announcer chose for itself
/// — typically the system hostname, and the same string the user puts
/// in their lan-mouse config's `hostname = "..."`. We key
/// [`PrimaryCache`] on this instead of the SRV target so the dialer
/// matches the config hostname even when the announcer's SRV target
/// has macOS-style suffixes (`Foo.local` vs `Foo-2.local`) or other
/// drift.
pub(crate) fn instance_from_fullname<'a>(fullname: &'a str, service_type: &str) -> &'a str {
    let suffix = format!(".{service_type}");
    fullname.strip_suffix(&suffix).unwrap_or(fullname)
}

/// Canonicalize a Bonjour/mDNS-SD name for cache lookup. Lower-cases,
/// drops a trailing FQDN dot, and drops the `.local` link-local
/// suffix. The `.local` domain is implied for everything mDNS-SD
/// touches, so callers shouldn't have to remember whether to include
/// it — config that says `Foo.local`, an announcer's instance label
/// `Foo`, and an SRV target `foo.local.` all collapse to `foo`.
pub(crate) fn normalize_mdns_name(s: &str) -> String {
    let s = strip_trailing_dot(s);
    let s = s.strip_suffix(".local").unwrap_or(s);
    let s = strip_bonjour_collision_suffix(s);
    s.to_ascii_lowercase()
}

pub(crate) fn is_usable_candidate_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(ip) if ip.is_unicast_link_local() => false,
        _ => true,
    }
}

/// Shared `peer_hostname -> primary_ipv4` map, populated by Discovery
/// and read by the dialer (`connect_to_handle`). Owned by the dialer
/// path so its references survive across discovery enable/disable
/// cycles — when the user toggles discovery off, the daemon stops
/// publishing/browsing but cached hints stay queryable. A subsequent
/// re-enable populates fresh entries into the same map.
pub(crate) type PrimaryCache = Rc<RefCell<HashMap<String, IpAddr>>>;
/// Shared `peer_fingerprint -> candidate_ips` map. This is the first
/// step toward making IP addresses a volatile transport detail: when
/// a configured client has `peer_fingerprint`, the dialer can follow
/// the mDNS-advertised addresses for that certificate even if the
/// hostname label changes.
pub(crate) type FingerprintCache = Rc<RefCell<HashMap<String, HashSet<IpAddr>>>>;

pub(crate) fn insert_fingerprint_candidate(
    cache: &FingerprintCache,
    fingerprint: &str,
    ip: IpAddr,
) {
    let fingerprint = normalize_fingerprint(fingerprint);
    if fingerprint.is_empty() || !is_usable_candidate_ip(ip) {
        return;
    }
    cache
        .borrow_mut()
        .entry(fingerprint)
        .or_default()
        .insert(ip);
}

#[derive(Default, Deserialize, Serialize)]
struct LastSuccessFile {
    fingerprints: HashMap<String, LastSuccessEntry>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum LastSuccessEntry {
    Timed {
        ips: HashSet<IpAddr>,
        updated_at_unix: u64,
    },
    Legacy(HashSet<IpAddr>),
}

impl LastSuccessEntry {
    fn ips(&self) -> &HashSet<IpAddr> {
        match self {
            Self::Timed { ips, .. } => ips,
            Self::Legacy(ips) => ips,
        }
    }

    fn is_expired(&self, now: u64) -> bool {
        match self {
            Self::Timed {
                updated_at_unix, ..
            } => now.saturating_sub(*updated_at_unix) > LAST_SUCCESS_TTL.as_secs(),
            Self::Legacy(_) => false,
        }
    }
}

fn now_unix_secs() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

pub(crate) fn load_last_success_candidates(
    cache: &FingerprintCache,
    path: &Path,
) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    let last_success = toml::from_str::<LastSuccessFile>(&raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let now = now_unix_secs()?;
    let mut loaded = 0usize;
    let mut expired = 0usize;
    for (fingerprint, entry) in last_success.fingerprints {
        if entry.is_expired(now) {
            expired += 1;
            continue;
        }
        for ip in entry.ips() {
            insert_fingerprint_candidate(cache, &fingerprint, *ip);
            loaded += 1;
        }
    }
    log::info!(
        "loaded {loaded} last-success peer address candidate(s) from {:?}",
        path
    );
    if expired > 0 {
        log::info!(
            "ignored {expired} expired last-success entries from {:?}",
            path
        );
    }
    Ok(())
}

pub(crate) fn persist_last_success_candidate(
    path: &Path,
    fingerprint: &str,
    ip: IpAddr,
) -> io::Result<()> {
    let fingerprint = normalize_fingerprint(fingerprint);
    if fingerprint.is_empty() {
        return Ok(());
    }
    let mut last_success = if path.exists() {
        let raw = fs::read_to_string(path)?;
        toml::from_str::<LastSuccessFile>(&raw).unwrap_or_default()
    } else {
        LastSuccessFile::default()
    };
    if !is_usable_candidate_ip(ip) {
        return Ok(());
    }
    let now = now_unix_secs()?;
    let mut ips = last_success
        .fingerprints
        .remove(&fingerprint)
        .map(|entry| entry.ips().clone())
        .unwrap_or_default();
    ips.insert(ip);
    last_success.fingerprints.insert(
        fingerprint,
        LastSuccessEntry::Timed {
            ips,
            updated_at_unix: now,
        },
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(&last_success)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, raw)
}

pub(crate) struct Discovery {
    /// The mDNS daemon. `None` when the subsystem is disabled (config
    /// toggle off, or daemon failed to start). All public methods are
    /// no-ops when this is None.
    daemon: Option<ServiceDaemon>,
    /// Fullname of our registered service, kept so we can unregister
    /// on shutdown / before re-registering.
    registered_fullname: Option<String>,
    /// Last registration identity. The periodic refresh loop can keep
    /// the current advertisement untouched while IP and port are
    /// unchanged.
    last_registered_primary: Option<Ipv4Addr>,
    last_registered_port: Option<u16>,
    /// Shared cache (see [`PrimaryCache`]).
    primary_cache: PrimaryCache,
    /// Shared cache (see [`FingerprintCache`]).
    fingerprint_cache: FingerprintCache,
    /// Local DTLS certificate fingerprint advertised as `fp=` so
    /// peers can eventually identify this machine independently of
    /// whichever IP address DHCP currently assigned.
    local_fingerprint: String,
    /// Background task that consumes browse events and updates
    /// `primary_cache`. Aborted when discovery is disabled or torn
    /// down.
    browse_task: Option<JoinHandle<()>>,
    /// Port the dialer should connect to (advertised in the SRV
    /// record's port field). Tracked so we can re-register when the
    /// listen port changes.
    port: u16,
}

impl Discovery {
    /// Construct a Discovery sharing `primary_cache` with the dialer.
    /// If `enabled` is false, returns an inert instance — calling any
    /// method on it is a no-op. Same outcome when the mDNS daemon
    /// fails to start (e.g. multicast group already joined by some
    /// other process, or the OS lacks the permissions). In both
    /// cases we log a warning and continue without discovery; the
    /// dialer falls back to plain hostname resolution.
    pub(crate) fn new(
        port: u16,
        enabled: bool,
        primary_cache: PrimaryCache,
        fingerprint_cache: FingerprintCache,
        local_fingerprint: String,
    ) -> Self {
        if !enabled {
            log::info!("mdns discovery disabled by config");
            return Self::inert(port, primary_cache, fingerprint_cache, local_fingerprint);
        }
        match ServiceDaemon::new() {
            Ok(daemon) => {
                let browse_task = start_browse(
                    &daemon,
                    primary_cache.clone(),
                    fingerprint_cache.clone(),
                    local_fingerprint.clone(),
                );
                let mut this = Self {
                    daemon: Some(daemon),
                    registered_fullname: None,
                    last_registered_primary: None,
                    last_registered_port: None,
                    primary_cache,
                    fingerprint_cache,
                    local_fingerprint,
                    browse_task,
                    port,
                };
                this.register();
                this
            }
            Err(e) => {
                log::warn!("mdns ServiceDaemon::new failed: {e}; discovery disabled");
                Self::inert(port, primary_cache, fingerprint_cache, local_fingerprint)
            }
        }
    }

    fn inert(
        port: u16,
        primary_cache: PrimaryCache,
        fingerprint_cache: FingerprintCache,
        local_fingerprint: String,
    ) -> Self {
        Self {
            daemon: None,
            registered_fullname: None,
            last_registered_primary: None,
            last_registered_port: None,
            primary_cache,
            fingerprint_cache,
            local_fingerprint,
            browse_task: None,
            port,
        }
    }

    /// Register `_lan-mouse._udp.local.` with our hostname + primary
    /// IP. Called on construction and again whenever the primary IP
    /// or port may have changed.
    fn register(&mut self) {
        let Some(daemon) = self.daemon.as_ref() else {
            return;
        };
        let host = local_hostname();
        let host_record = format!("{host}.local.");
        let primary = match primary_ipv4() {
            Some(ip) => ip,
            None => {
                log::warn!(
                    "mdns: no default-route interface; skipping registration (will retry on \
                     interface change)"
                );
                return;
            }
        };
        if self.registered_fullname.is_some()
            && self.last_registered_primary == Some(primary)
            && self.last_registered_port == Some(self.port)
        {
            return;
        }
        let mut props = HashMap::new();
        props.insert(TXT_PRIMARY_KEY.to_string(), primary.to_string());
        props.insert(
            TXT_FINGERPRINT_KEY.to_string(),
            self.local_fingerprint.clone(),
        );
        let info = match ServiceInfo::new(
            SERVICE_TYPE,
            &host,
            &host_record,
            IpAddr::V4(primary),
            self.port,
            Some(props),
        ) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("mdns ServiceInfo::new failed: {e}; skipping registration");
                return;
            }
        };
        let fullname = info.get_fullname().to_string();
        // Drop the old registration after the new service info is
        // buildable. A transient interface read failure must not tear
        // down a still-valid advertisement.
        if let Some(old) = self.registered_fullname.take() {
            let _ = daemon.unregister(&old);
        }
        match daemon.register(info) {
            Ok(()) => {
                log::info!(
                    "mdns: registered {fullname} on {primary}:{port} (primary interface, fp={fp})",
                    port = self.port,
                    fp = self.local_fingerprint.as_str(),
                );
                self.registered_fullname = Some(fullname);
                self.last_registered_primary = Some(primary);
                self.last_registered_port = Some(self.port);
            }
            Err(e) => {
                self.last_registered_primary = None;
                self.last_registered_port = None;
                log::warn!("mdns register failed: {e}");
            }
        }
    }

    /// Re-register with the current primary IP. Called periodically
    /// by the service's main loop so the TXT record reflects the
    /// active default-route interface even when interface changes
    /// don't arrive through if-watch.
    pub(crate) fn refresh(&mut self) {
        self.register();
    }

    /// Re-register with a new port (config changed).
    pub(crate) fn set_port(&mut self, port: u16) {
        if self.port == port {
            return;
        }
        self.port = port;
        self.refresh();
    }

    /// Toggle the subsystem on/off. Off → unregister, abort browse,
    /// drop daemon. On → spin up afresh, reusing the same shared
    /// cache so any prior hints stay queryable until overwritten.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        let currently = self.daemon.is_some();
        if currently == enabled {
            return;
        }
        if enabled {
            *self = Self::new(
                self.port,
                true,
                self.primary_cache.clone(),
                self.fingerprint_cache.clone(),
                self.local_fingerprint.clone(),
            );
        } else {
            self.shutdown();
        }
    }

    fn shutdown(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            if let Some(name) = self.registered_fullname.take() {
                let _ = daemon.unregister(&name);
            }
            let _ = daemon.shutdown();
        }
        self.last_registered_primary = None;
        self.last_registered_port = None;
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        // Don't clear primary_cache: the dialer may still benefit
        // from the last-known hints, and a re-enable would otherwise
        // lose them until each peer's next announcement.
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn a background task that browses `_lan-mouse._udp.local.` and
/// keeps `primary_cache` updated as ServiceResolved / ServiceRemoved
/// events arrive.
fn start_browse(
    daemon: &ServiceDaemon,
    primary_cache: Rc<RefCell<HashMap<String, IpAddr>>>,
    fingerprint_cache: Rc<RefCell<HashMap<String, HashSet<IpAddr>>>>,
    local_fingerprint: String,
) -> Option<JoinHandle<()>> {
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("mdns browse failed: {e}");
            return None;
        }
    };
    let local_fingerprint = normalize_fingerprint(&local_fingerprint);
    let local_name = normalize_mdns_name(&local_hostname());
    Some(spawn_local(async move {
        let mut seen_services: HashMap<String, (IpAddr, u16, Option<String>)> = HashMap::new();
        while let Ok(event) = receiver.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(resolved) => {
                    let Some(primary_str) = resolved.get_property_val_str(TXT_PRIMARY_KEY) else {
                        continue;
                    };
                    let Ok(ip) = primary_str.parse::<IpAddr>() else {
                        log::debug!(
                            "mdns: peer {} advertised malformed primary={primary_str:?}",
                            resolved.get_fullname()
                        );
                        continue;
                    };
                    let instance = instance_from_fullname(resolved.get_fullname(), SERVICE_TYPE);
                    let key = normalize_mdns_name(instance);
                    let target = strip_trailing_dot(resolved.get_hostname());
                    let fingerprint = resolved.get_property_val_str(TXT_FINGERPRINT_KEY);
                    let normalized_fingerprint = fingerprint.map(normalize_fingerprint);
                    if normalized_fingerprint.as_deref() == Some(local_fingerprint.as_str())
                        || (key == local_name && normalize_mdns_name(target) == local_name)
                    {
                        log::debug!("mdns: ignoring our own service announcement {key}");
                        continue;
                    }
                    let signature = (ip, resolved.get_port(), normalized_fingerprint.clone());
                    let first_or_changed = seen_services.get(&key) != Some(&signature);
                    if first_or_changed {
                        log::info!(
                            "mdns: peer instance={key} (target={target}) announces primary={ip} \
                             (port={port}, fp={fingerprint:?})",
                            port = resolved.get_port(),
                        );
                    } else {
                        log::debug!(
                            "mdns: peer instance={key} refresh primary={ip} (port={port})",
                            port = resolved.get_port(),
                        );
                    }
                    seen_services.insert(key.clone(), signature);
                    primary_cache.borrow_mut().insert(key, ip);
                    if let Some(fingerprint) = normalized_fingerprint {
                        if !fingerprint.is_empty() {
                            let mut candidates = resolved
                                .get_addresses()
                                .iter()
                                .map(|addr| addr.to_ip_addr())
                                .filter(|ip| is_usable_candidate_ip(*ip))
                                .collect::<HashSet<_>>();
                            if is_usable_candidate_ip(ip) {
                                candidates.insert(ip);
                            }
                            fingerprint_cache
                                .borrow_mut()
                                .insert(fingerprint, candidates);
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    // Best-effort: the fullname is "<instance>._lan-
                    // mouse._udp.local." — we don't have the host
                    // record handy here, so drop on next browse-
                    // resolved instead of trying to map back. Keeps
                    // the cache slightly stale on goodbye but never
                    // wrong: if the peer comes back with a different
                    // primary, the next ServiceResolved overwrites.
                    log::debug!("mdns: service removed {fullname}");
                }
                _ => {}
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn persists_and_loads_last_success_candidates() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lan-mouse-last-success-{nonce}.toml"));
        let cache: FingerprintCache = Default::default();

        persist_last_success_candidate(&path, " AA:BB ", "192.168.10.155".parse().unwrap())
            .expect("persist candidate");
        load_last_success_candidates(&cache, &path).expect("load candidates");

        let loaded = cache.borrow();
        let ips = loaded.get("aa:bb").expect("fingerprint cache entry");
        assert!(ips.contains(&"192.168.10.155".parse().unwrap()));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn normalizes_bonjour_collision_suffix() {
        assert_eq!(
            normalize_mdns_name("sangwha-KWEN (2).local."),
            "sangwha-kwen"
        );
    }

    #[test]
    fn skips_link_local_ipv6_candidates() {
        let cache: FingerprintCache = Default::default();
        insert_fingerprint_candidate(&cache, "aa:bb", "fe80::1".parse().unwrap());
        assert!(cache.borrow().is_empty());
    }

    #[test]
    fn ignores_expired_last_success_candidates() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("lan-mouse-last-success-expired-{nonce}.toml"));
        let stale = now_unix_secs().expect("clock") - LAST_SUCCESS_TTL.as_secs() - 1;
        fs::write(
            &path,
            format!(
                r#"
                [fingerprints."aa:bb"]
                ips = ["192.168.10.155"]
                updated_at_unix = {stale}
                "#
            ),
        )
        .expect("write stale cache");

        let cache: FingerprintCache = Default::default();
        load_last_success_candidates(&cache, &path).expect("load candidates");
        assert!(cache.borrow().is_empty());

        let _ = fs::remove_file(path);
    }
}
