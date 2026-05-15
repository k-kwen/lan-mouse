use crate::{
    actions,
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    discovery::{self, Discovery, FingerprintCache, PrimaryCache},
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use lan_mouse_ipc::{
    ActionTrigger, AsyncFrontendListener, ClientAction, ClientHandle, FrontendEvent,
    FrontendRequest, IpcError, IpcListenerCreationError, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
    #[error("health check failed: {0}")]
    Health(String),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// fingerprint-indexed address candidates learned from mDNS,
    /// successful outgoing connections, and observed incoming peers.
    fingerprint_cache: FingerprintCache,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// consecutive health ticks where capture was disabled
    capture_recovery_attempts: u8,
    /// consecutive health ticks where emulation was disabled
    emulation_recovery_attempts: u8,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    /// Default-client handles whose enter DDC action was already
    /// started on CaptureBegin because the peer was ready. The ACK
    /// path skips native DDC for these handles but still runs hooks
    /// and any non-prefired future actions.
    prefired_enter_ddc: HashSet<ClientHandle>,
    next_trigger_handle: u64,
    /// mDNS-SD service registration + browse. Advertises our primary
    /// interface IP for peer dialers to bias toward; populates
    /// shared `PrimaryCache` (read by `LanMouseConnection`) from
    /// peer announcements.
    discovery: Discovery,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));
        // listener + connection. The primary-IP cache is owned by
        // the dialer side so its references survive Discovery
        // toggles; Discovery writes peer hints into it as browse
        // events arrive.
        let listener =
            LanMouseListener::new(config.port(), cert.clone(), authorized_keys.clone()).await?;
        let primary_cache: PrimaryCache = Default::default();
        let fingerprint_cache: FingerprintCache = Default::default();
        let last_success_cache_path = config.last_success_cache_path();
        if let Err(e) =
            discovery::load_last_success_candidates(&fingerprint_cache, &last_success_cache_path)
        {
            log::warn!(
                "failed to load last-success cache from {:?}: {e}",
                last_success_cache_path
            );
        }
        let conn = LanMouseConnection::new(
            cert.clone(),
            client_manager.clone(),
            primary_cache.clone(),
            fingerprint_cache.clone(),
            Some(last_success_cache_path),
        );

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(
            capture_backend,
            conn,
            config.release_bind(),
            config.release_threshold_px(),
        );
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();
        let discovery = Discovery::new(
            port,
            config.mdns_discovery(),
            primary_cache,
            fingerprint_cache.clone(),
            public_key_fingerprint.clone(),
        );
        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            fingerprint_cache,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            capture_recovery_attempts: 0,
            emulation_recovery_attempts: 0,
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            prefired_enter_ddc: Default::default(),
            next_trigger_handle: 0,
            discovery,
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // Periodic refresh of the Discovery service registration so
        // its TXT record stays accurate when the OS-preferred
        // interface (default route) changes — e.g. user switches
        // off Wi-Fi and Mac falls back to Ethernet. Cheap: at most
        // one re-publish every 30s, and a no-op when the primary
        // hasn't moved. `Skip` so a long suspend doesn't backlog-
        // burst on resume.
        let mut discovery_refresh_tick = tokio::time::interval(Duration::from_secs(30));
        discovery_refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // skip the immediate-fire of the first tick — Discovery
        // already published once at startup
        discovery_refresh_tick.tick().await;
        let mut health_tick = tokio::time::interval(Duration::from_secs(60));
        health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // skip the immediate-fire; startup events will establish the
        // initial capture/emulation status first.
        health_tick.tick().await;

        let mut terminal_error = None;
        loop {
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                _ = discovery_refresh_tick.tick() => self.discovery.refresh(),
                _ = health_tick.tick() => {
                    if let Err(e) = self.health_check() {
                        log::error!("{e}");
                        terminal_error = Some(e);
                        break;
                    }
                },
                r = signal::ctrl_c() => {
                    if let Err(e) = r {
                        terminal_error = Some(e.into());
                    }
                    break;
                },
            }
        }

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        if let Some(e) = terminal_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePeerFingerprint(handle, peer_fingerprint) => {
                self.update_peer_fingerprint(handle, peer_fingerprint);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook);
                self.save_config();
            }
            FrontendRequest::UpdateLeaveHook(handle, leave_hook) => {
                self.update_leave_hook(handle, leave_hook);
                self.save_config();
            }
            FrontendRequest::UpdateActions(handle, actions) => {
                self.update_actions(handle, actions);
                self.save_config();
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::SetReleaseThreshold(threshold) => {
                self.config.set_release_threshold_px(threshold);
                self.capture.set_release_threshold(threshold);
                self.notify_frontend(FrontendEvent::ReleaseThreshold(threshold));
                self.save_config();
            }
            FrontendRequest::SetMdnsDiscovery(enabled) => {
                self.config.set_mdns_discovery(enabled);
                self.discovery.set_enabled(enabled);
                self.notify_frontend(FrontendEvent::MdnsDiscovery(enabled));
                self.save_config();
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                peer_fingerprint: c.peer_fingerprint,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
                leave_hook: c.cmd_leave,
                actions: c.actions,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let release_threshold = self.config.release_threshold_px();
        self.capture.set_release_threshold(release_threshold);
        self.notify_frontend(FrontendEvent::ReleaseThreshold(release_threshold));
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                self.remember_peer_addr(&fingerprint, addr);
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.discovery.set_port(port);
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release_for_handover(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.remember_peer_addr(&fingerprint, addr);
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            EmulationEvent::PeerHello {
                addr,
                fingerprint,
                commit,
            } => {
                // Map the peer's source addr back to its client handle
                // and stamp the commit. Fingerprint is preferred so
                // `ips = []` clients still get matched after DHCP
                // changes; addr fallback keeps legacy clients working.
                let handle = fingerprint
                    .as_deref()
                    .and_then(|fp| self.client_manager.get_client_by_peer_fingerprint(fp))
                    .or_else(|| self.client_manager.get_client(addr));
                if let Some(handle) = handle {
                    self.client_manager.set_peer_commit(handle, Some(commit));
                    self.broadcast_client(handle);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                } else {
                    self.prefire_enter_ddc_if_ready(handle);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                let skip_prefired_ddc = self.prefired_enter_ddc.remove(&handle);
                self.spawn_hook_command(handle, HookKind::Enter, skip_prefired_ddc);
            }
            ICaptureEvent::ClientLeft(handle) => {
                self.prefired_enter_ddc.remove(&handle);
                self.spawn_hook_command(handle, HookKind::Leave, false);
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        self.notify_frontend(FrontendEvent::ReleaseThreshold(
            self.config.release_threshold_px(),
        ));
        self.notify_frontend(FrontendEvent::MdnsDiscovery(self.config.mdns_discovery()));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn health_check(&mut self) -> Result<(), ServiceError> {
        const MAX_RECOVERY_ATTEMPTS: u8 = 3;
        let active_clients = self.client_manager.active_clients().len();
        let incoming = self.incoming_conns.len();
        log::info!(
            "health: capture={:?}, emulation={:?}, active_clients={active_clients}, \
             incoming={incoming}, pending_frontend_events={pending}, \
             capture_recovery_attempts={capture_attempts}, \
             emulation_recovery_attempts={emulation_attempts}",
            self.capture_status,
            self.emulation_status,
            pending = self.pending_frontend_events.len(),
            capture_attempts = self.capture_recovery_attempts,
            emulation_attempts = self.emulation_recovery_attempts,
        );
        if self.capture_status == Status::Disabled {
            self.capture_recovery_attempts = self.capture_recovery_attempts.saturating_add(1);
            log::warn!(
                "health: capture disabled; requesting re-enable (attempt {}/{MAX_RECOVERY_ATTEMPTS})",
                self.capture_recovery_attempts,
            );
            self.capture.reenable();
            if self.capture_recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
                return Err(ServiceError::Health(format!(
                    "capture backend stayed disabled after {MAX_RECOVERY_ATTEMPTS} recovery attempts"
                )));
            }
        } else {
            self.capture_recovery_attempts = 0;
        }
        if self.emulation_status == Status::Disabled {
            self.emulation_recovery_attempts = self.emulation_recovery_attempts.saturating_add(1);
            log::warn!(
                "health: emulation disabled; requesting re-enable (attempt {}/{MAX_RECOVERY_ATTEMPTS})",
                self.emulation_recovery_attempts,
            );
            self.emulation.reenable();
            if self.emulation_recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
                return Err(ServiceError::Health(format!(
                    "emulation backend stayed disabled after {MAX_RECOVERY_ATTEMPTS} recovery attempts"
                )));
            }
        } else {
            self.emulation_recovery_attempts = 0;
        }
        Ok(())
    }

    fn remember_peer_addr(&self, fingerprint: &str, addr: SocketAddr) {
        discovery::insert_fingerprint_candidate(&self.fingerprint_cache, fingerprint, addr.ip());
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        if self
            .client_manager
            .remove_client(handle)
            .map(|(_, s)| s.active)
            .unwrap_or(false)
        {
            self.capture.destroy(handle);
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        if self.client_manager.set_hostname(handle, hostname.clone()) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_peer_fingerprint(&mut self, handle: ClientHandle, peer_fingerprint: Option<String>) {
        let peer_fingerprint = peer_fingerprint
            .map(|fp| crypto::normalize_fingerprint(&fp))
            .filter(|fp| !fp.is_empty());
        log::info!("peer fingerprint changed: {peer_fingerprint:?}");
        self.client_manager
            .set_peer_fingerprint(handle, peer_fingerprint);
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn update_leave_hook(&mut self, handle: ClientHandle, leave_hook: Option<String>) {
        self.client_manager.set_leave_hook(handle, leave_hook);
        self.broadcast_client(handle);
    }

    fn update_actions(&mut self, handle: ClientHandle, actions: Vec<ClientAction>) {
        self.client_manager.set_actions(handle, actions);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn client_ready_for_prefire(&self, handle: ClientHandle) -> bool {
        self.client_manager.active_addr(handle).is_some() && self.client_manager.alive(handle)
    }

    fn prefire_enter_ddc_if_ready(&mut self, handle: ClientHandle) {
        self.prefired_enter_ddc.remove(&handle);
        if !self.client_ready_for_prefire(handle) {
            return;
        }
        let actions = self
            .client_manager
            .get_actions(handle)
            .into_iter()
            .filter(actions::is_fast_enter_prefire_supported)
            .collect::<Vec<_>>();
        if actions.is_empty() {
            return;
        }
        self.prefired_enter_ddc.insert(handle);
        log::info!("prefiring enter DDC for ready client {handle}");
        Self::spawn_actions(actions, "prefire enter");
    }

    fn spawn_hook_command(&self, handle: ClientHandle, kind: HookKind, skip_prefired_ddc: bool) {
        let actions = self
            .client_manager
            .get_actions(handle)
            .into_iter()
            .filter(|action| action_trigger(action) == kind.action_trigger())
            .filter(|action| {
                !(skip_prefired_ddc && actions::is_fast_enter_prefire_supported(action))
            })
            .collect::<Vec<_>>();
        let cmd = match kind {
            HookKind::Enter => self.client_manager.get_enter_cmd(handle),
            HookKind::Leave => self.client_manager.get_leave_cmd(handle),
        };
        if actions.is_empty() && cmd.is_none() {
            return;
        }
        let label = kind.label();
        Self::spawn_actions_and_hook(actions, cmd, label);
    }

    fn spawn_actions(actions: Vec<ClientAction>, label: &'static str) {
        tokio::task::spawn_local(async move {
            for action in actions {
                log::info!("running {label} action: {action:?}");
                match actions::run(action).await {
                    Ok(()) => log::info!("{label} action completed successfully"),
                    Err(e) => log::warn!("{label} action failed: {e}"),
                }
            }
        });
    }

    fn spawn_actions_and_hook(
        actions: Vec<ClientAction>,
        cmd: Option<String>,
        label: &'static str,
    ) {
        tokio::task::spawn_local(async move {
            for action in actions {
                log::info!("running {label} action: {action:?}");
                match actions::run(action).await {
                    Ok(()) => log::info!("{label} action completed successfully"),
                    Err(e) => log::warn!("{label} action failed: {e}"),
                }
            }
            let Some(cmd) = cmd else { return };
            log::info!("spawning {label} hook: {cmd}");
            #[cfg(windows)]
            let spawn_res = Command::new("cmd").arg("/C").arg(cmd.as_str()).spawn();
            #[cfg(not(windows))]
            let spawn_res = Command::new("sh").arg("-c").arg(cmd.as_str()).spawn();
            let mut child = match spawn_res {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute {label} hook `{cmd}`: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{label} hook `{cmd}` exited successfully");
                    } else {
                        log::warn!("{label} hook `{cmd}` exited with {s}");
                    }
                }
                Err(e) => log::warn!("{label} hook `{cmd}`: {e}"),
            }
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    Enter,
    Leave,
}

impl HookKind {
    fn label(self) -> &'static str {
        match self {
            HookKind::Enter => "enter",
            HookKind::Leave => "leave",
        }
    }

    fn action_trigger(self) -> ActionTrigger {
        match self {
            HookKind::Enter => ActionTrigger::Enter,
            HookKind::Leave => ActionTrigger::Leave,
        }
    }
}

fn action_trigger(action: &ClientAction) -> ActionTrigger {
    match action {
        ClientAction::DdcVcp { on, .. } => *on,
    }
}
