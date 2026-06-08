use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto::{generate_fingerprint, normalize_fingerprint};
use crate::discovery::{
    self, FingerprintCache, PrimaryCache, insert_fingerprint_candidate, is_usable_candidate_ip,
    normalize_mdns_name,
};
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
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
    sync::{
        Mutex,
        mpsc::{self, Receiver, Sender, error::TrySendError},
    },
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
const INBOUND_EVENT_QUEUE_CAPACITY: usize = 1024;

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
#[derive(Debug, Clone)]
struct ConnectionAttemptState {
    connecting: bool,
    next_attempt_at: Instant,
    backoff: Duration,
    signature: u64,
}

type AttemptStates = Rc<RefCell<HashMap<ClientHandle, ConnectionAttemptState>>>;

fn signature_of(ips: &HashSet<IpAddr>, primary: Option<IpAddr>) -> u64 {
    let mut sorted: Vec<IpAddr> = ips.iter().copied().collect();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    primary.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn is_droppable_inbound_event(event: &ProtoEvent) -> bool {
    matches!(
        event,
        ProtoEvent::Input(input_event::Event::Pointer(
            input_event::PointerEvent::Motion { .. }
        ))
    )
}

async fn send_received_event(
    tx: &Sender<(ClientHandle, ProtoEvent)>,
    handle: ClientHandle,
    event: ProtoEvent,
) -> bool {
    if is_droppable_inbound_event(&event) {
        match tx.try_send((handle, event)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::debug!("dropping pointer motion for client {handle}: inbound queue full");
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    } else {
        tx.send((handle, event)).await.is_ok()
    }
}

fn reserve_attempt_slot(
    attempts: &mut HashMap<ClientHandle, ConnectionAttemptState>,
    handle: ClientHandle,
    signature: u64,
    now: Instant,
) -> bool {
    match attempts.get_mut(&handle) {
        None => {
            attempts.insert(
                handle,
                ConnectionAttemptState {
                    connecting: true,
                    next_attempt_at: now,
                    backoff: INITIAL_RETRY_BACKOFF,
                    signature,
                },
            );
            true
        }
        Some(state) if state.connecting => false,
        Some(state) if state.signature != signature => {
            state.connecting = true;
            state.signature = signature;
            state.next_attempt_at = now;
            state.backoff = INITIAL_RETRY_BACKOFF;
            true
        }
        Some(state) if now >= state.next_attempt_at => {
            state.connecting = true;
            true
        }
        Some(_) => false,
    }
}

fn record_attempt_failure_with_signature(
    attempts: &mut HashMap<ClientHandle, ConnectionAttemptState>,
    handle: ClientHandle,
    signature: u64,
    now: Instant,
) {
    let entry = attempts.entry(handle).or_insert(ConnectionAttemptState {
        connecting: false,
        next_attempt_at: now,
        backoff: INITIAL_RETRY_BACKOFF,
        signature,
    });
    entry.connecting = false;
    entry.signature = signature;
    let next = entry.backoff;
    entry.next_attempt_at = now + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
}

/// Update `attempt_states[handle]` after a failed connect attempt:
/// clears the in-flight marker, doubles the backoff (capped at
/// [`MAX_RETRY_BACKOFF`]), and stamps the candidate-set signature so a
/// later signature change can short-circuit the gate.
fn record_attempt_failure(
    attempt_states: &AttemptStates,
    handle: ClientHandle,
    ips: &HashSet<IpAddr>,
    primary: Option<IpAddr>,
) {
    let sig = signature_of(ips, primary);
    let mut attempts = attempt_states.borrow_mut();
    record_attempt_failure_with_signature(&mut attempts, handle, sig, Instant::now());
}

fn clear_attempt_state(attempt_states: &AttemptStates, handle: ClientHandle) {
    attempt_states.borrow_mut().remove(&handle);
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
    /// Per-handle connect-attempt state. Tracks both in-flight
    /// attempts and retry backoff in one map so reservation, cooldown,
    /// and completion cannot drift across separate locks.
    attempt_states: AttemptStates,
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
        let (recv_tx, recv_rx) = mpsc::channel(INBOUND_EVENT_QUEUE_CAPACITY);
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            attempt_states: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            primary_hints,
            fingerprint_hints,
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
                Ok(_) => {
                    log::trace!("{event} >->->->->- {addr}");
                    Ok(())
                }
                Err(e) => {
                    log::warn!("client {handle} failed to send: {e}");
                    disconnect(&self.client_manager, handle, addr, &conn, &self.conns).await;
                    Err(e.into())
                }
            }
        } else {
            self.ensure_connected(handle).await;
            Err(LanMouseConnectionError::NotConnected)
        }
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
        if self.reserve_attempt(handle) {
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.attempt_states.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
                self.primary_hints.clone(),
                self.fingerprint_hints.clone(),
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
    fn reserve_attempt(&self, handle: ClientHandle) -> bool {
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
        reserve_attempt_slot(
            &mut self.attempt_states.borrow_mut(),
            handle,
            sig,
            Instant::now(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    attempt_states: AttemptStates,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    primary_hints: PrimaryCache,
    fingerprint_hints: FingerprintCache,
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
        // Refuse to dial without a pinned peer fingerprint. The dialer sets
        // `insecure_skip_verify`, so an unpinned outbound DTLS session performs
        // NO server authentication and would stream locally-captured input to
        // whatever host answers at the (possibly spoofed) address — a MITM
        // keystroke-exfiltration risk. Mirror the listener's default-deny
        // posture; the `pair` flow always records a fingerprint.
        if expected_fingerprint.is_none() {
            log::warn!(
                "client {handle} has no peer_fingerprint; refusing to connect \
                 (run `lan-mouse pair` or set peer_fingerprint to enable authenticated outbound)"
            );
            record_attempt_failure(&attempt_states, handle, &ips_set, primary_ip);
            return Err(LanMouseConnectionError::NotConnected);
        }
        log::info!("client ({handle}) connecting ... (ips: {addrs:?}, preferred: {preferred:?})");
        if addrs.is_empty() && preferred.is_none() {
            // Nothing to dial. Bump backoff and bail without spawning
            // DTLS work or spamming logs on every subsequent mouse
            // event — `reserve_attempt` will keep gating until either
            // the backoff elapses or new info arrives.
            record_attempt_failure(&attempt_states, handle, &ips_set, primary_ip);
            return Err(LanMouseConnectionError::NotConnected);
        }
        let res = connect_any(&addrs, preferred, cert, expected_fingerprint.clone()).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                record_attempt_failure(&attempt_states, handle, &ips_set, primary_ip);
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
        clear_attempt_state(&attempt_states, handle);

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
    clear_attempt_state(&attempt_states, handle);
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
    while let Ok(len) = conn.recv(&mut buf).await {
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
        match ProtoEvent::try_from(&buf[..len]) {
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
                        if !send_received_event(&tx, handle, event).await {
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
    if let Err(e) = conn.close().await {
        log::debug!("failed to close connection for client ({handle}) @ {addr}: {e}");
    }
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::any::Any;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct FailingSendConn {
        closed: AtomicBool,
        addr: SocketAddr,
    }

    #[async_trait]
    impl Conn for FailingSendConn {
        async fn connect(&self, _addr: SocketAddr) -> webrtc_util::Result<()> {
            Ok(())
        }

        async fn recv(&self, _buf: &mut [u8]) -> webrtc_util::Result<usize> {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unused").into())
        }

        async fn recv_from(&self, _buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unused").into())
        }

        async fn send(&self, _buf: &[u8]) -> webrtc_util::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "send failed").into())
        }

        async fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> webrtc_util::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "send failed").into())
        }

        fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
            Ok(self.addr)
        }

        fn remote_addr(&self) -> Option<SocketAddr> {
            Some(self.addr)
        }

        async fn close(&self) -> webrtc_util::Result<()> {
            self.closed.store(true, Ordering::Release);
            Ok(())
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync) {
            self
        }
    }

    #[test]
    fn attempt_slot_rejects_duplicate_while_connecting() {
        let mut attempts = HashMap::new();
        let now = Instant::now();

        assert!(reserve_attempt_slot(&mut attempts, 7, 11, now));
        assert!(!reserve_attempt_slot(&mut attempts, 7, 11, now));
        assert!(attempts.get(&7).is_some_and(|state| state.connecting));
    }

    #[test]
    fn attempt_failure_sets_backoff_and_allows_after_cooldown() {
        let mut attempts = HashMap::new();
        let now = Instant::now();

        assert!(reserve_attempt_slot(&mut attempts, 7, 11, now));
        record_attempt_failure_with_signature(&mut attempts, 7, 11, now);

        assert!(!reserve_attempt_slot(
            &mut attempts,
            7,
            11,
            now + Duration::from_millis(500)
        ));
        assert!(reserve_attempt_slot(
            &mut attempts,
            7,
            11,
            now + INITIAL_RETRY_BACKOFF
        ));
    }

    #[test]
    fn attempt_signature_change_bypasses_backoff() {
        let mut attempts = HashMap::new();
        let now = Instant::now();

        assert!(reserve_attempt_slot(&mut attempts, 7, 11, now));
        record_attempt_failure_with_signature(&mut attempts, 7, 11, now);

        assert!(reserve_attempt_slot(
            &mut attempts,
            7,
            12,
            now + Duration::from_millis(100)
        ));
        let state = attempts.get(&7).expect("attempt state");
        assert_eq!(state.signature, 12);
        assert_eq!(state.backoff, INITIAL_RETRY_BACKOFF);
        assert!(state.connecting);
    }

    #[test]
    fn only_pointer_motion_is_droppable_under_backpressure() {
        use input_event::{Event, KeyboardEvent, PointerEvent};

        assert!(is_droppable_inbound_event(&ProtoEvent::Input(
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.0,
                dy: 1.0,
            })
        )));
        assert!(!is_droppable_inbound_event(&ProtoEvent::Input(
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: input_event::BTN_LEFT,
                state: 0,
            })
        )));
        assert!(!is_droppable_inbound_event(&ProtoEvent::Input(
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 0,
            })
        )));
        assert!(!is_droppable_inbound_event(&ProtoEvent::Leave(0)));
    }

    #[tokio::test]
    async fn send_failure_is_returned_to_caller() {
        let manager = ClientManager::default();
        let handle = manager.add_client();
        manager.set_alive(handle, true);
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("socket addr");
        manager.set_active_addr(handle, Some(addr));
        manager.set_fix_ips(handle, vec![addr.ip()]);

        let conn = Arc::new(FailingSendConn {
            closed: AtomicBool::new(false),
            addr,
        });
        let (recv_tx, recv_rx) = mpsc::channel(INBOUND_EVENT_QUEUE_CAPACITY);
        let lm = LanMouseConnection {
            cert: Certificate::generate_self_signed(vec!["lan-mouse-test".to_owned()])
                .expect("test certificate"),
            client_manager: manager,
            conns: Rc::new(Mutex::new(HashMap::from([(
                addr,
                conn.clone() as Arc<dyn Conn + Send + Sync>,
            )]))),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            primary_hints: Default::default(),
            fingerprint_hints: Default::default(),
            attempt_states: Default::default(),
            last_success_cache_path: None,
        };

        let err = lm
            .send(ProtoEvent::Ping, handle)
            .await
            .expect_err("send should fail");

        assert!(matches!(err, LanMouseConnectionError::Webrtc(_)));
        assert!(conn.closed.load(Ordering::Acquire));
    }
}
