use clap::{Args, Parser, Subcommand, ValueEnum};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ActionTrigger, AsyncFrontendEventReader, AsyncFrontendRequestWriter, ClientAction,
    ClientConfig, ClientHandle, ClientState, ConnectionError, FrontendEvent, FrontendRequest,
    IpcError, Position, connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    peer_fingerprint: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliActionTrigger {
    Enter,
    Leave,
}

impl From<CliActionTrigger> for ActionTrigger {
    fn from(trigger: CliActionTrigger) -> Self {
        match trigger {
            CliActionTrigger::Enter => ActionTrigger::Enter,
            CliActionTrigger::Leave => ActionTrigger::Leave,
        }
    }
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change expected peer certificate fingerprint
    SetPeerFingerprint {
        id: ClientHandle,
        fingerprint: Option<String>,
    },
    /// change enter hook
    SetEnterHook {
        id: ClientHandle,
        hook: Option<String>,
    },
    /// change leave hook
    SetLeaveHook {
        id: ClientHandle,
        hook: Option<String>,
    },
    /// append a DDC/VCP native action
    AddDdcVcpAction {
        id: ClientHandle,
        #[arg(value_enum)]
        on: CliActionTrigger,
        #[arg(long)]
        monitor: Option<String>,
        #[arg(long, value_parser = parse_u8_auto)]
        code: u8,
        #[arg(long, value_parser = parse_u32_auto)]
        value: u32,
    },
    /// remove all native actions for a client
    ClearActions { id: ClientHandle },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            peer_fingerprint,
            port,
            ips,
            enter_hook,
            leave_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(peer_fingerprint) = peer_fingerprint {
                        tx.request(FrontendRequest::UpdatePeerFingerprint(
                            handle,
                            Some(peer_fingerprint),
                        ))
                        .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    if let Some(leave_hook) = leave_hook {
                        tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config.hostname.unwrap_or("unknown".to_owned());
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        let peer = config
                            .peer_fingerprint
                            .map(|fp| format!(", peer_fingerprint: {fp}"))
                            .unwrap_or_default();
                        let enter_hook = config
                            .cmd
                            .map(|cmd| format!(", enter_hook: {cmd:?}"))
                            .unwrap_or_default();
                        let leave_hook = config
                            .cmd_leave
                            .map(|cmd| format!(", leave_hook: {cmd:?}"))
                            .unwrap_or_default();
                        let actions = if config.actions.is_empty() {
                            String::new()
                        } else {
                            format!(", actions: {:?}", config.actions)
                        };
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}{peer}{enter_hook}{leave_hook}{actions}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPeerFingerprint { id, fingerprint } => {
            tx.request(FrontendRequest::UpdatePeerFingerprint(id, fingerprint))
                .await?
        }
        CliSubcommand::SetEnterHook { id, hook } => {
            tx.request(FrontendRequest::UpdateEnterHook(id, hook))
                .await?
        }
        CliSubcommand::SetLeaveHook { id, hook } => {
            tx.request(FrontendRequest::UpdateLeaveHook(id, hook))
                .await?
        }
        CliSubcommand::AddDdcVcpAction {
            id,
            on,
            monitor,
            code,
            value,
        } => {
            let Some((config, _)) = current_config_for(&mut rx, &mut tx, id).await? else {
                return Ok(());
            };
            let mut actions = config.actions;
            actions.push(ClientAction::DdcVcp {
                on: on.into(),
                monitor,
                code,
                value,
            });
            tx.request(FrontendRequest::UpdateActions(id, actions))
                .await?
        }
        CliSubcommand::ClearActions { id } => {
            tx.request(FrontendRequest::UpdateActions(id, Vec::new()))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
    }
    Ok(())
}

async fn current_config_for(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
    id: ClientHandle,
) -> Result<Option<(ClientConfig, ClientState)>, CliError> {
    tx.request(FrontendRequest::Enumerate()).await?;
    while let Some(e) = rx.next().await {
        if let FrontendEvent::Enumerate(clients) = e? {
            return Ok(clients
                .into_iter()
                .find(|(handle, _, _)| *handle == id)
                .map(|(_, config, state)| (config, state)));
        }
    }
    Ok(None)
}

fn parse_u8_auto(value: &str) -> Result<u8, String> {
    parse_u32_auto(value)
        .and_then(|v| u8::try_from(v).map_err(|_| format!("{value:?} is outside u8 range")))
}

fn parse_u32_auto(value: &str) -> Result<u32, String> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        value.parse::<u32>().map_err(|e| e.to_string())
    }
}
