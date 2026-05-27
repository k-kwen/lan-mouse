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
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{UnboundedSender, unbounded_channel},
    task::{JoinHandle, spawn_local},
};

use crate::crypto::normalize_fingerprint;

pub(crate) const SERVICE_TYPE: &str = "_lan-mouse._udp.local.";
pub(crate) const TXT_PRIMARY_KEY: &str = "primary";
pub(crate) const TXT_FINGERPRINT_KEY: &str = "fp";
const LAST_SUCCESS_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const FALLBACK_PROBE_PORT: u16 = 4243;
const FALLBACK_PROBE_MAGIC: &str = "lan-mouse-fp-probe/1";

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

fn is_cgnat_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

fn is_lan_discovery_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_unspecified()
        && !ip.is_broadcast()
        && !ip.is_multicast()
        && !is_cgnat_ipv4(ip)
}

fn is_tailscale_interface_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("tailscale")
}

fn local_lan_ipv4_addrs() -> Vec<IpAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces,
        Err(e) => {
            log::warn!("get_if_addrs failed for discovery address enumeration: {e}");
            return Vec::new();
        }
    };
    let mut addrs = ifaces
        .into_iter()
        .filter(|iface| !is_tailscale_interface_name(&iface.name))
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) if is_lan_discovery_ipv4(v4.ip) => Some(IpAddr::V4(v4.ip)),
            _ => None,
        })
        .collect::<Vec<_>>();
    addrs.sort();
    addrs.dedup();
    addrs
}

fn local_lan_broadcast_targets() -> Vec<SocketAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces,
        Err(e) => {
            log::warn!("get_if_addrs failed for fallback probe targets: {e}");
            return vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::BROADCAST),
                FALLBACK_PROBE_PORT,
            )];
        }
    };
    let mut targets = ifaces
        .into_iter()
        .filter(|iface| !is_tailscale_interface_name(&iface.name))
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) if is_lan_discovery_ipv4(v4.ip) => v4
                .broadcast
                .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
                .map(IpAddr::V4),
            _ => None,
        })
        .map(|ip| SocketAddr::new(ip, FALLBACK_PROBE_PORT))
        .collect::<Vec<_>>();
    targets.push(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::BROADCAST),
        FALLBACK_PROBE_PORT,
    ));
    targets.sort();
    targets.dedup();
    targets
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

fn is_usable_discovery_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_lan_discovery_ipv4(ip),
        IpAddr::V6(_) => false,
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
) -> bool {
    let fingerprint = normalize_fingerprint(fingerprint);
    if fingerprint.is_empty() || !is_usable_discovery_ip(ip) {
        return false;
    }
    cache
        .borrow_mut()
        .entry(fingerprint)
        .or_default()
        .insert(ip)
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
    if !is_usable_discovery_ip(ip) {
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
    last_registered_addrs: Vec<IpAddr>,
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
                    last_registered_addrs: Vec::new(),
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
            last_registered_addrs: Vec::new(),
            last_registered_port: None,
            primary_cache,
            fingerprint_cache,
            local_fingerprint,
            browse_task: None,
            port,
        }
    }

    /// Register `_lan-mouse._udp.local.` with all usable LAN IPv4
    /// addresses. `primary=` stays a TXT hint for connection
    /// preference, but the A records now cover every eligible local
    /// interface so peers on a non-default subnet can still discover
    /// this daemon.
    fn register(&mut self) {
        let Some(daemon) = self.daemon.as_ref() else {
            return;
        };
        let host = local_hostname();
        let host_record = format!("{host}.local.");
        let addrs = local_lan_ipv4_addrs();
        let primary = match primary_ipv4()
            .filter(|ip| addrs.contains(&IpAddr::V4(*ip)))
            .or_else(|| {
                addrs.iter().find_map(|ip| match ip {
                    IpAddr::V4(ip) => Some(*ip),
                    IpAddr::V6(_) => None,
                })
            }) {
            Some(ip) => ip,
            None => {
                log::warn!(
                    "mdns: no usable LAN IPv4 addresses; skipping registration (will retry on \
                     interface change)"
                );
                return;
            }
        };
        if self.registered_fullname.is_some()
            && self.last_registered_primary == Some(primary)
            && self.last_registered_addrs == addrs
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
            addrs.as_slice(),
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
                    "mdns: registered {fullname} on {addrs:?}:{port} (primary={primary}, fp={fp})",
                    port = self.port,
                    fp = self.local_fingerprint.as_str(),
                );
                self.registered_fullname = Some(fullname);
                self.last_registered_primary = Some(primary);
                self.last_registered_addrs = addrs;
                self.last_registered_port = Some(self.port);
            }
            Err(e) => {
                self.last_registered_primary = None;
                self.last_registered_addrs.clear();
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
        self.last_registered_addrs.clear();
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
        let mut seen_services: HashMap<String, (IpAddr, Vec<IpAddr>, u16, Option<String>)> =
            HashMap::new();
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
                    let mut candidates = resolved
                        .get_addresses()
                        .iter()
                        .map(|addr| addr.to_ip_addr())
                        .filter(|ip| is_usable_discovery_ip(*ip))
                        .collect::<Vec<_>>();
                    if is_usable_discovery_ip(ip) {
                        candidates.push(ip);
                    }
                    candidates.sort();
                    candidates.dedup();
                    let signature = (
                        ip,
                        candidates.clone(),
                        resolved.get_port(),
                        normalized_fingerprint.clone(),
                    );
                    let first_or_changed = seen_services.get(&key) != Some(&signature);
                    if first_or_changed {
                        log::info!(
                            "mdns: peer instance={key} (target={target}) announces primary={ip} \
                             candidates={candidates:?} (port={port}, fp={fingerprint:?})",
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
                            let candidates = candidates.into_iter().collect::<HashSet<_>>();
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

enum FallbackProbeCommand {
    Query { fingerprint: String },
    SetDtlsPort(u16),
}

/// Very low-rate fallback for networks where mDNS browse misses a
/// peer after DHCP or subnet changes. It does not run a scan loop of
/// its own: callers enqueue a query only when an active peer has no
/// active connection and no static/DNS candidates. Existing mDNS or
/// last-success hints do not suppress the probe because they may be
/// stale after DHCP changes.
pub(crate) struct FallbackProbe {
    tx: Option<UnboundedSender<FallbackProbeCommand>>,
    task: Option<JoinHandle<()>>,
}

impl FallbackProbe {
    pub(crate) fn new(
        dtls_port: u16,
        local_fingerprint: String,
        fingerprint_cache: FingerprintCache,
    ) -> Self {
        let (tx, mut rx) = unbounded_channel::<FallbackProbeCommand>();
        let local_fingerprint = normalize_fingerprint(&local_fingerprint);
        let task = spawn_local(async move {
            let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), FALLBACK_PROBE_PORT);
            let socket = match UdpSocket::bind(bind_addr).await {
                Ok(socket) => socket,
                Err(e) => {
                    log::warn!(
                        "fallback discovery probe disabled: failed to bind {bind_addr}: {e}"
                    );
                    return;
                }
            };
            if let Err(e) = socket.set_broadcast(true) {
                log::warn!("fallback discovery probe cannot enable broadcast: {e}");
            }
            log::info!("fallback discovery probe listening on {bind_addr}");

            let mut dtls_port = dtls_port;
            let mut buf = [0u8; 512];
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(FallbackProbeCommand::Query { fingerprint }) => {
                            send_fallback_probe(&socket, &local_fingerprint, &fingerprint).await;
                        }
                        Some(FallbackProbeCommand::SetDtlsPort(port)) => {
                            dtls_port = port;
                        }
                        None => break,
                    },
                    result = socket.recv_from(&mut buf) => match result {
                        Ok((len, src)) => {
                            handle_fallback_probe_packet(
                                &socket,
                                &fingerprint_cache,
                                &local_fingerprint,
                                dtls_port,
                                &buf[..len],
                                src,
                            ).await;
                        }
                        Err(e) => log::debug!("fallback discovery probe recv failed: {e}"),
                    },
                }
            }
        });
        Self {
            tx: Some(tx),
            task: Some(task),
        }
    }

    pub(crate) fn query(&self, fingerprint: &str) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let fingerprint = normalize_fingerprint(fingerprint);
        if fingerprint.is_empty() {
            return;
        }
        let _ = tx.send(FallbackProbeCommand::Query { fingerprint });
    }

    pub(crate) fn set_dtls_port(&self, port: u16) {
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(FallbackProbeCommand::SetDtlsPort(port));
        }
    }

    pub(crate) fn terminate(&mut self) {
        self.tx.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for FallbackProbe {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn send_fallback_probe(socket: &UdpSocket, local_fingerprint: &str, fingerprint: &str) {
    let fingerprint = normalize_fingerprint(fingerprint);
    if fingerprint.is_empty() || fingerprint == local_fingerprint {
        return;
    }
    let payload = fallback_probe_request(local_fingerprint, &fingerprint);
    let targets = local_lan_broadcast_targets();
    for target in targets {
        if let Err(e) = socket.send_to(payload.as_bytes(), target).await {
            log::debug!("fallback discovery probe send to {target} failed: {e}");
        }
    }
}

async fn handle_fallback_probe_packet(
    socket: &UdpSocket,
    fingerprint_cache: &FingerprintCache,
    local_fingerprint: &str,
    dtls_port: u16,
    payload: &[u8],
    src: SocketAddr,
) {
    if !is_usable_discovery_ip(src.ip()) {
        return;
    }
    let Ok(payload) = std::str::from_utf8(payload) else {
        return;
    };
    if payload.lines().next() != Some(FALLBACK_PROBE_MAGIC) {
        return;
    }

    if let Some(want) = fallback_probe_field(payload, "want") {
        let want = normalize_fingerprint(want);
        let from = fallback_probe_field(payload, "from").map(normalize_fingerprint);
        if want == local_fingerprint && from.as_deref() != Some(local_fingerprint) {
            let response = fallback_probe_response(local_fingerprint, dtls_port);
            if let Err(e) = socket.send_to(response.as_bytes(), src).await {
                log::debug!("fallback discovery probe response to {src} failed: {e}");
            }
        }
        return;
    }

    if let Some(have) = fallback_probe_field(payload, "have") {
        let have = normalize_fingerprint(have);
        if have.is_empty() || have == local_fingerprint {
            return;
        }
        if insert_fingerprint_candidate(fingerprint_cache, &have, src.ip()) {
            log::info!(
                "fallback discovery: peer {have} is reachable at {}",
                src.ip()
            );
        }
    }
}

fn fallback_probe_request(local_fingerprint: &str, wanted_fingerprint: &str) -> String {
    format!(
        "{FALLBACK_PROBE_MAGIC}\nfrom={}\nwant={}\n",
        normalize_fingerprint(local_fingerprint),
        normalize_fingerprint(wanted_fingerprint)
    )
}

fn fallback_probe_response(local_fingerprint: &str, dtls_port: u16) -> String {
    format!(
        "{FALLBACK_PROBE_MAGIC}\nhave={}\nport={dtls_port}\n",
        normalize_fingerprint(local_fingerprint)
    )
}

fn fallback_probe_field<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    payload
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
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
    fn skips_tailscale_cgnat_discovery_candidates() {
        let cache: FingerprintCache = Default::default();
        insert_fingerprint_candidate(&cache, "aa:bb", "100.76.35.84".parse().unwrap());
        assert!(cache.borrow().is_empty());
    }

    #[test]
    fn accepts_private_lan_discovery_candidates() {
        let cache: FingerprintCache = Default::default();
        insert_fingerprint_candidate(&cache, "aa:bb", "192.168.11.152".parse().unwrap());
        let loaded = cache.borrow();
        let ips = loaded.get("aa:bb").expect("fingerprint cache entry");
        assert!(ips.contains(&"192.168.11.152".parse().unwrap()));
    }

    #[test]
    fn fallback_probe_messages_are_field_parseable() {
        let request = fallback_probe_request(" AA:BB ", " CC:DD ");
        assert_eq!(request.lines().next(), Some(FALLBACK_PROBE_MAGIC));
        assert_eq!(fallback_probe_field(&request, "from"), Some("aa:bb"));
        assert_eq!(fallback_probe_field(&request, "want"), Some("cc:dd"));

        let response = fallback_probe_response(" AA:BB ", 4242);
        assert_eq!(response.lines().next(), Some(FALLBACK_PROBE_MAGIC));
        assert_eq!(fallback_probe_field(&response, "have"), Some("aa:bb"));
        assert_eq!(fallback_probe_field(&response, "port"), Some("4242"));
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
