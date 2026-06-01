use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto::{generate_fingerprint, normalize_fingerprint};
use crate::discovery::{
    self, FingerprintCache, PrimaryCache, insert_fingerprint_candidate, is_usable_candidate_ip,
    normalize_mdns_name,
};
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_blocking, spawn_local},
};
use webrtc_dtls::{
    Error as DtlsError,
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial backoff between connect attempts that find no usable address
/// (no static IPs, no DNS-resolved IPs, no mDNS primary hint). Doubles
/// on each subsequent failure up to [`MAX_RETRY_BACKOFF`]. The backoff
/// is bypassed entirely when the input set changes (e.g. mDNS browse
/// resolves a primary, DNS lookup returns IPs) so a peer that comes
/// back online reconnects on the next mouse event without waiting.
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Per-handle gate that throttles repeat connect attempts when nothing
/// new is available to dial. `signature` hashes the candidate set we
/// last attempted; if the current set differs we skip the gate and
/// retry immediately. Otherwise `next_attempt_at` enforces exponential
/// backoff capped at [`MAX_RETRY_BACKOFF`].
struct RetryState {
    next_attempt_at: Instant,
    backoff: Duration,
    signature: u64,
}

fn signature_of(ips: &HashSet<IpAddr>, primary: Option<IpAddr>) -> u64 {
    let mut sorted: Vec<IpAddr> = ips.iter().copied().collect();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    primary.hash(&mut hasher);
    hasher.finish()
}

/// Update `retry_state[handle]` after a failed connect attempt: doubles
/// the backoff (capped at [`MAX_RETRY_BACKOFF`]) and stamps the
/// candidate-set signature so a later signature change can short-
/// circuit the gate.
fn record_retry_failure(
    retry_state: &Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    handle: ClientHandle,
    ips: &HashSet<IpAddr>,
    primary: Option<IpAddr>,
) {
    let sig = signature_of(ips, primary);
    let mut map = retry_state.borrow_mut();
    let entry = map.entry(handle).or_insert(RetryState {
        next_attempt_at: Instant::now(),
        backoff: INITIAL_RETRY_BACKOFF,
        signature: sig,
    });
    entry.signature = sig;
    let next = entry.backoff;
    entry.next_attempt_at = Instant::now() + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
}

fn discovery_hint_for(
    client_manager: &ClientManager,
    handle: ClientHandle,
    primary_hints: &PrimaryCache,
    fingerprint_hints: &FingerprintCache,
) -> Option<IpAddr> {
    client_manager
        .get_hostname(handle)
        .and_then(|h| {
            let key = normalize_mdns_name(&h);
            primary_hints.borrow().get(&key).copied()
        })
        .or_else(|| {
            client_manager.get_peer_fingerprint(handle).and_then(|fp| {
                let key = normalize_fingerprint(&fp);
                fingerprint_hints.borrow().get(&key).and_then(|candidates| {
                    candidates
                        .iter()
                        .copied()
                        .find(|ip| is_usable_candidate_ip(*ip))
                })
            })
        })
}

fn discovery_candidates_for(
    client_manager: &ClientManager,
    handle: ClientHandle,
    fingerprint_hints: &FingerprintCache,
) -> HashSet<IpAddr> {
    client_manager
        .get_peer_fingerprint(handle)
        .and_then(|fp| {
            let key = normalize_fingerprint(&fp);
            fingerprint_hints.borrow().get(&key).map(|candidates| {
                candidates
                    .iter()
                    .copied()
                    .filter(|ip| is_usable_candidate_ip(*ip))
                    .collect()
            })
        })
        .unwrap_or_default()
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    expected_fingerprint: Option<String>,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let verify_peer_certificate = expected_fingerprint.map(|expected| {
        Arc::new(
            move |certificates: &[Vec<u8>],
                  _chains: &[rustls::pki_types::CertificateDer<'static>]| {
                let Some(cert) = certificates.first() else {
                    return Err(DtlsError::Other(
                        "peer did not present a certificate".to_owned(),
                    ));
                };
                let actual = generate_fingerprint(cert);
                if actual == expected {
                    Ok(())
                } else {
                    Err(DtlsError::Other(format!(
                        "peer fingerprint mismatch: expected {expected}, got {actual}"
                    )))
                }
            },
        ) as _
    });
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        verify_peer_certificate,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

/// Time the preferred address gets to handshake alone before the
/// rest of the candidate list joins the race. Modeled on RFC 8305
/// "happy eyeballs" v6→v4 fallback delay; long enough that a healthy
/// preferred address virtually always wins, short enough that a
/// broken preferred path only slightly delays connect.
const PREFERRED_ADDR_HEAD_START: Duration = Duration::from_millis(200);

async fn connect_any(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
    cert: Certificate,
    expected_fingerprint: Option<String>,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    if let Some(p) = preferred {
        // Dial the peer's mDNS-advertised primary first. If it
        // handshakes within `PREFERRED_ADDR_HEAD_START` we're done
        // before the others even start — the dialer biases toward
        // the OS-preferred interface (Mac service order, Linux
        // default route) without relying on RTT racing alone.
        joinset.spawn_local(connect(p, cert.clone(), expected_fingerprint.clone()));
        let head_start = tokio::time::sleep(PREFERRED_ADDR_HEAD_START);
        tokio::pin!(head_start);
        loop {
            tokio::select! {
                _ = &mut head_start => break,
                Some(r) = joinset.join_next() => match r.expect("join error") {
                    Ok(conn) => return Ok(conn),
                    Err((a, e)) => log::warn!("failed to connect to {a}: `{e}`"),
                },
            }
        }
    }
    for &addr in addrs {
        if Some(addr) == preferred {
            // already racing; don't dial the same socket twice
            continue;
        }
        joinset.spawn_local(connect(addr, cert.clone(), expected_fingerprint.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// Map of `peer_hostname -> primary_ipv4` populated by the
    /// `Discovery` mDNS browse task. Read on every `connect_to_handle`
    /// to bias which address gets the handshake head-start. Empty
    /// when discovery is disabled or no peer hint has arrived yet.
    primary_hints: PrimaryCache,
    /// Map of `peer_fingerprint -> candidate_ips` populated by mDNS.
    /// This makes the configured certificate identity the preferred
    /// discovery key when available; hostname is only the fallback.
    fingerprint_hints: FingerprintCache,
    /// Per-handle retry gate. Suppresses connect spawns when the
    /// previous attempt failed and nothing new is available to dial,
    /// so an offline peer doesn't trigger a fresh `connect_to_handle`
    /// (and the associated DNS / mDNS lookup churn) on every mouse
    /// event. Cleared on successful connect; bypassed automatically
    /// when the candidate-set signature changes.
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    /// Persistent last-success cache path. Successful DTLS handshakes
    /// write the peer fingerprint -> IP candidate here so a daemon
    /// restart keeps dynamic-IP recovery warm even before fresh mDNS
    /// browse events arrive.
    last_success_cache_path: Option<PathBuf>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        primary_hints: PrimaryCache,
        fingerprint_hints: FingerprintCache,
        last_success_cache_path: Option<PathBuf>,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            primary_hints,
            fingerprint_hints,
            retry_state: Default::default(),
            last_success_cache_path,
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some((addr, conn)) = self.conn_for_handle(handle).await {
            if !self.client_manager.alive(handle) {
                return Err(LanMouseConnectionError::TargetEmulationDisabled);
            }
            match conn.send(buf).await {
                Ok(_) => {}
                Err(e) => {
                    log::warn!("client {handle} failed to send: {e}");
                    disconnect(&self.client_manager, handle, addr, &conn, &self.conns).await;
                }
            }
            log::trace!("{event} >->->->->- {addr}");
            return Ok(());
        }

        self.ensure_connected(handle).await;
        Err(LanMouseConnectionError::NotConnected)
    }

    async fn conn_for_handle(
        &self,
        handle: ClientHandle,
    ) -> Option<(SocketAddr, Arc<dyn Conn + Send + Sync>)> {
        let addr = self.client_manager.active_addr(handle)?;
        let conn = {
            let conns = self.conns.lock().await;
            conns.get(&addr).cloned()
        }?;
        Some((addr, conn))
    }

    pub(crate) async fn is_connected(&self, handle: ClientHandle) -> bool {
        self.conn_for_handle(handle).await.is_some()
    }

    pub(crate) async fn is_ready(&self, handle: ClientHandle) -> bool {
        self.conn_for_handle(handle).await.is_some() && self.client_manager.alive(handle)
    }

    pub(crate) async fn ensure_connected(&self, handle: ClientHandle) -> bool {
        if self.is_connected(handle).await {
            return true;
        }
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) && self.should_attempt(handle) {
            connecting.insert(handle);
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
                self.primary_hints.clone(),
                self.fingerprint_hints.clone(),
                self.retry_state.clone(),
                self.last_success_cache_path.clone(),
            ));
        }
        false
    }

    /// Decide whether to spawn another `connect_to_handle` for `handle`.
    /// Returns true (and refreshes the recorded signature) when:
    ///   - we have no prior attempt for this handle, or
    ///   - the candidate-set signature has changed since the last
    ///     attempt (new IP from DNS, or new mDNS primary), or
    ///   - the recorded backoff has elapsed.
    ///
    /// Otherwise returns false; the caller treats this as "still in
    /// cooldown, keep returning NotConnected silently."
    fn should_attempt(&self, handle: ClientHandle) -> bool {
        let mut ips = self.client_manager.get_ips(handle).unwrap_or_default();
        ips.retain(|ip| is_usable_candidate_ip(*ip));
        ips.extend(discovery_candidates_for(
            &self.client_manager,
            handle,
            &self.fingerprint_hints,
        ));
        let primary = discovery_hint_for(
            &self.client_manager,
            handle,
            &self.primary_hints,
            &self.fingerprint_hints,
        );
        let sig = signature_of(&ips, primary);
        let mut state = self.retry_state.borrow_mut();
        match state.get_mut(&handle) {
            None => true,
            Some(s) if s.signature != sig => {
                s.signature = sig;
                s.next_attempt_at = Instant::now();
                s.backoff = INITIAL_RETRY_BACKOFF;
                true
            }
            Some(s) => Instant::now() >= s.next_attempt_at,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    primary_hints: PrimaryCache,
    fingerprint_hints: FingerprintCache,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    last_success_cache_path: Option<PathBuf>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(mut ips_set) = client_manager.get_ips(handle) {
        ips_set.retain(|ip| is_usable_candidate_ip(*ip));
        ips_set.extend(discovery_candidates_for(
            &client_manager,
            handle,
            &fingerprint_hints,
        ));
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = ips_set
            .iter()
            .copied()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        // mDNS-advertised primary IP for this peer, if known. Used
        // by `connect_any` as a head-start address: the dialer races
        // it alone for ~200ms before joining the rest of the list,
        // so a healthy primary almost always wins regardless of
        // raw RTT ordering.
        let primary_ip =
            discovery_hint_for(&client_manager, handle, &primary_hints, &fingerprint_hints);
        let preferred = primary_ip.map(|ip| SocketAddr::new(ip, port));
        let expected_fingerprint = client_manager
            .get_peer_fingerprint(handle)
            .map(|fp| normalize_fingerprint(&fp));
        log::info!("client ({handle}) connecting ... (ips: {addrs:?}, preferred: {preferred:?})");
        if addrs.is_empty() && preferred.is_none() {
            // Nothing to dial. Bump backoff and bail without spawning
            // DTLS work or spamming logs on every subsequent mouse
            // event — `should_attempt` will keep gating until either
            // the backoff elapses or new info arrives.
            record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
            connecting.lock().await.remove(&handle);
            return Err(LanMouseConnectionError::NotConnected);
        }
        let res = connect_any(&addrs, preferred, cert, expected_fingerprint.clone()).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        if let Some(fingerprint) = expected_fingerprint.as_deref() {
            insert_fingerprint_candidate(&fingerprint_hints, fingerprint, addr.ip());
            if let Some(path) = last_success_cache_path.as_ref() {
                let path = path.clone();
                let fingerprint = fingerprint.to_owned();
                let ip = addr.ip();
                spawn_blocking(move || {
                    if let Err(e) =
                        discovery::persist_last_success_candidate(&path, &fingerprint, ip)
                    {
                        log::warn!("failed to persist last-success candidate to {path:?}: {e}");
                    }
                });
            }
        }
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, false);
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);
        retry_state.borrow_mut().remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            ping_response.clone(),
            expected_fingerprint,
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    expected_fingerprint: Option<String>,
) {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    while conn.recv(&mut buf).await.is_ok() {
        let current_fingerprint = client_manager
            .get_peer_fingerprint(handle)
            .map(|fp| normalize_fingerprint(&fp));
        if current_fingerprint != expected_fingerprint {
            log::warn!(
                "closing stale session for client {handle} @ {addr}: expected fingerprint changed \
                 from {expected_fingerprint:?} to {current_fingerprint:?}"
            );
            let _ = conn.close().await;
            break;
        }
        match buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => {
                        if tx.send((handle, event)).is_err() {
                            log::debug!(
                                "receive loop for client {handle} @ {addr} stopped: channel closed"
                            );
                            break;
                        }
                    }
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    disconnect(&client_manager, handle, addr, &conn, &conns).await;
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: &Arc<dyn Conn + Send + Sync>,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    let removed_current = {
        let mut conns = conns.lock().await;
        if conns
            .get(&addr)
            .is_some_and(|current| Arc::ptr_eq(current, conn))
        {
            conns.remove(&addr);
            true
        } else {
            false
        }
    };
    if client_manager.active_addr(handle) == Some(addr) {
        client_manager.set_active_addr(handle, None);
        client_manager.set_alive(handle, false);
        client_manager.set_peer_commit(handle, None);
    } else if !removed_current {
        log::debug!(
            "stale connection for client ({handle}) @ {addr} closed after a newer session was installed"
        );
    }
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}
