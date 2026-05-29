use std::{
    collections::{HashMap, HashSet},
    io,
    net::IpAddr,
    time::{Duration, Instant},
};

use clap::Args;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Serialize;
use thiserror::Error;

use crate::{
    config::{Config, ConfigClient},
    crypto::normalize_fingerprint,
    discovery::{
        SERVICE_TYPE, TXT_FINGERPRINT_KEY, TXT_PRIMARY_KEY, instance_from_fullname,
        strip_trailing_dot,
    },
};
use lan_mouse_ipc::{ActionTrigger, ClientAction, DEFAULT_PORT, Position};

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct DiscoverArgs {
    /// discovery window in milliseconds
    #[arg(long, default_value_t = 2500)]
    timeout_ms: u64,

    /// print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct PairArgs {
    /// Mac key: the peer certificate fingerprint to trust
    #[arg(long = "mac-key", alias = "peer-fingerprint")]
    mac_key: String,

    /// where the Mac screen sits relative to this Windows screen
    #[arg(long)]
    position: Position,

    /// optional hostname label; discovery fills this when omitted
    #[arg(long)]
    hostname: Option<String>,

    /// label stored under [authorized_fingerprints]
    #[arg(long, default_value = "mac")]
    label: String,

    /// peer listen port
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// discovery window in milliseconds before writing config
    #[arg(long, default_value_t = 2500)]
    discover_timeout_ms: u64,

    /// skip mDNS lookup and write the supplied values only
    #[arg(long)]
    no_discover: bool,

    /// optional enter hook command
    #[arg(long)]
    enter_hook: Option<String>,

    /// optional leave hook command
    #[arg(long)]
    leave_hook: Option<String>,

    /// native DDC monitor selector for input-source switching
    #[arg(long)]
    ddc_monitor: Option<String>,

    /// VCP code for native DDC switching; 0x60 is input source
    #[arg(long, default_value_t = 0x60)]
    ddc_code: u8,

    /// DDC input-source value to apply when entering the peer screen
    #[arg(long)]
    ddc_enter_input: Option<u32>,

    /// DDC input-source value to apply when leaving the peer screen
    #[arg(long)]
    ddc_leave_input: Option<u32>,
}

#[derive(Debug, Error)]
pub enum PairingError {
    #[error(transparent)]
    Mdns(#[from] mdns_sd::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoveredPeer {
    pub instance: String,
    pub hostname: String,
    pub port: u16,
    pub fingerprint: Option<String>,
    pub primary: Option<IpAddr>,
    pub addresses: Vec<IpAddr>,
}

pub async fn discover_command(args: DiscoverArgs) -> Result<(), PairingError> {
    let peers = discover(Duration::from_millis(args.timeout_ms)).await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&peers).expect("serialize discovered peers")
        );
    } else if peers.is_empty() {
        println!("no lan-mouse peers discovered");
    } else {
        for peer in peers {
            let fp = peer.fingerprint.as_deref().unwrap_or("-");
            let primary = peer
                .primary
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "-".to_owned());
            println!(
                "{} host={} port={} fp={} primary={} addrs={:?}",
                peer.instance, peer.hostname, peer.port, fp, primary, peer.addresses
            );
        }
    }
    Ok(())
}

pub async fn pair_command(mut config: Config, args: PairArgs) -> Result<(), PairingError> {
    let mac_key = normalize_fingerprint(&args.mac_key);
    let discovered = if args.no_discover {
        vec![]
    } else {
        discover(Duration::from_millis(args.discover_timeout_ms)).await?
    };
    let matched = discovered.iter().find(|peer| {
        peer.fingerprint
            .as_deref()
            .map(|fp| normalize_fingerprint(fp) == mac_key)
            .unwrap_or(false)
    });

    let hostname = args
        .hostname
        .or_else(|| matched.map(|peer| peer.hostname.clone()));
    let port = matched.map(|peer| peer.port).unwrap_or(args.port);
    let actions = ddc_actions(
        args.ddc_monitor,
        args.ddc_code,
        args.ddc_enter_input,
        args.ddc_leave_input,
    );
    let mut clients = config.clients();
    let new_client = ConfigClient {
        ips: HashSet::new(),
        hostname: hostname.clone(),
        peer_fingerprint: Some(mac_key.clone()),
        port,
        pos: args.position,
        active: true,
        enter_hook: args.enter_hook,
        leave_hook: args.leave_hook,
        actions,
    };

    if let Some(existing) = clients.iter_mut().find(|client| {
        client
            .peer_fingerprint
            .as_deref()
            .map(|fp| normalize_fingerprint(fp) == mac_key)
            .unwrap_or(false)
    }) {
        *existing = new_client;
    } else {
        clients.push(new_client);
    }
    config.set_port(port);
    config.set_clients(clients);

    let mut authorized = config.authorized_fingerprints();
    authorized.insert(mac_key.clone(), args.label);
    config.set_authorized_keys(authorized);
    config.set_mdns_discovery(true);
    config.write_back()?;

    if let Some(peer) = matched {
        println!(
            "paired {} ({}) by fingerprint {}; config={:?}",
            peer.instance,
            peer.hostname,
            mac_key,
            config.config_path()
        );
    } else {
        println!(
            "paired fingerprint {}; no matching mDNS peer was seen during discovery; config={:?}",
            mac_key,
            config.config_path()
        );
    }
    Ok(())
}

fn ddc_actions(
    monitor: Option<String>,
    code: u8,
    enter_input: Option<u32>,
    leave_input: Option<u32>,
) -> Vec<ClientAction> {
    let Some(monitor) = monitor else {
        return vec![];
    };
    let mut actions = vec![];
    if let Some(value) = enter_input {
        actions.push(ClientAction::DdcVcp {
            on: ActionTrigger::Enter,
            monitor: Some(monitor.clone()),
            code,
            value,
        });
    }
    if let Some(value) = leave_input {
        actions.push(ClientAction::DdcVcp {
            on: ActionTrigger::Leave,
            monitor: Some(monitor),
            code,
            value,
        });
    }
    actions
}

async fn discover(timeout: Duration) -> Result<Vec<DiscoveredPeer>, PairingError> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut peers = HashMap::<String, DiscoveredPeer>::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(event) = tokio::time::timeout(remaining, receiver.recv_async()).await else {
            break;
        };
        match event {
            Ok(ServiceEvent::ServiceResolved(resolved)) => {
                let instance =
                    instance_from_fullname(resolved.get_fullname(), SERVICE_TYPE).to_owned();
                let hostname = strip_trailing_dot(resolved.get_hostname()).to_owned();
                let fingerprint = resolved
                    .get_property_val_str(TXT_FINGERPRINT_KEY)
                    .map(normalize_fingerprint)
                    .filter(|fp| !fp.is_empty());
                let primary = resolved
                    .get_property_val_str(TXT_PRIMARY_KEY)
                    .and_then(|value| value.parse::<IpAddr>().ok());
                let mut addresses = resolved
                    .get_addresses()
                    .iter()
                    .map(|addr| addr.to_ip_addr())
                    .collect::<Vec<_>>();
                if let Some(primary) = primary {
                    addresses.push(primary);
                }
                addresses.sort();
                addresses.dedup();
                peers.insert(
                    resolved.get_fullname().to_owned(),
                    DiscoveredPeer {
                        instance,
                        hostname,
                        port: resolved.get_port(),
                        fingerprint,
                        primary,
                        addresses,
                    },
                );
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();

    let mut peers = peers.into_values().collect::<Vec<_>>();
    peers.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(peers)
}
