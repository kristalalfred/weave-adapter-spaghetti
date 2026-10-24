mod config;
mod daemon;
mod hops;
mod inventory;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::{Json, Router, routing::get};
use clap::Parser;
use reqwest::{Client, RequestBuilder, StatusCode};
use spaghetti_common::ipc_protocol::{Destination, RouteInfo, SourceInfo};
use tokio::task::JoinHandle;
use weave_core::auth::{self, Token};
use weave_core::logging::ErrorChain;
use weave_core::{
    AdapterDescriptor, AdapterKind, AudioCodec, AudioConstraint, Container, DeclaredNode,
    DesiredHop, FormatConstraint, HopEndpointClass, HopProfile, HopStatus, LinkClass,
    LinkDescription, LocalEnd, MediaBehavior, NodeCapabilities, NodeDescriptor, NodeHeartbeat,
    NodeRegistration, NodeStatus, PROTOCOL_VERSION, RoleSet, SessionDescription,
    SignallingExchange, SignallingReply, Transport, TransportClass,
};
use weave_reach::{Reach, STUN_SERVERS};

use config::AdapterConfig;
use daemon::{SpaghettiClient, SpaghettiError};
use hops::{Action, Plan, Rates, Refusals, Retries, SourceChange};
use inventory::Inventory;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(
    name = "weave-adapter-spaghetti",
    version,
    about = "open-weave southbound adapter for spaghetti"
)]
struct Args {
    #[arg(help = "Path to the adapter config file (YAML)")]
    #[arg(long, env = "WEAVE_NODE_CONFIG", hide_env_values = true)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    weave_core::logging::init();

    let args = Args::parse();
    let config = AdapterConfig::load(&args.config)?;
    let token = config.node.resolve_southbound_token()?;
    if token.is_none() {
        tracing::warn!(
            "{}=1: calling southbound without authentication",
            auth::AUTH_DISABLED_VAR
        );
    }
    let southbound = Southbound::new(config.node.southbound_url.clone(), token)?;
    let spaghetti = SpaghettiClient::new(&config.spaghetti.socket);
    let health_server = spawn_health_server(config.node.listen.clone());
    let reach = if config.node.topology.site.is_some() {
        Some(Reach::start(&STUN_SERVERS).await)
    } else {
        None
    };

    tracing::info!(
        node_id = %config.node.id,
        southbound_url = %config.node.southbound_url,
        socket = %config.spaghetti.socket.display(),
        topology = ?config.node.topology,
        "spaghetti adapter starting"
    );

    let result = tokio::select! {
        result = weave_core::shutdown::signal() => {
            result.context("waiting for shutdown signal")?;
            tracing::info!("spaghetti adapter shutting down");
            Ok(())
        }
        result = sync_loop(&southbound, &spaghetti, &config, reach.as_ref()) => result,
    };
    health_server.abort();
    result
}

struct Southbound {
    http: Client,
    url: String,
    token: Option<Token>,
}

impl Southbound {
    fn new(url: String, token: Option<Token>) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .context("building the HTTP client")?,
            url: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.header(reqwest::header::AUTHORIZATION, token.header_value()),
            None => request,
        }
    }

    async fn register(&self, registration: &NodeRegistration) -> Result<()> {
        let response = self
            .request(self.http.post(format!("{}/nodes/register", self.url)))
            .json(registration)
            .send()
            .await
            .context("registering with southbound")?;
        let status = response.status();
        if status.is_client_error() {
            let body = response.text().await.unwrap_or_default();
            return Err(RegistrationRejected { status, body }.into());
        }
        response
            .error_for_status()
            .context("registering with southbound")?;
        Ok(())
    }

    async fn desired(&self, node_id: &str) -> Result<Option<Vec<DesiredHop>>> {
        let response = self
            .request(
                self.http
                    .get(format!("{}/nodes/{node_id}/desired", self.url)),
            )
            .send()
            .await
            .context("fetching desired hops")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let hops: Vec<DesiredHop> = response
            .error_for_status()
            .context("fetching desired hops")?
            .json()
            .await
            .context("decoding desired hops")?;
        anyhow::ensure!(
            hops.iter().all(|hop| hop.node_id == node_id),
            "southbound returned another node's desired hops"
        );
        Ok(Some(hops))
    }

    async fn exchange(
        &self,
        node_id: &str,
        exchange: &SignallingExchange,
    ) -> Result<SignallingReply> {
        self.request(
            self.http
                .post(format!("{}/nodes/{node_id}/signalling", self.url)),
        )
        .json(exchange)
        .send()
        .await
        .context("exchanging peer descriptions")?
        .error_for_status()
        .context("exchanging peer descriptions")?
        .json()
        .await
        .context("decoding peer descriptions")
    }

    async fn heartbeat(&self, heartbeat: &NodeHeartbeat) -> Result<bool> {
        let response = self
            .request(self.http.post(format!(
                "{}/nodes/{}/heartbeat",
                self.url, heartbeat.node_id
            )))
            .json(heartbeat)
            .send()
            .await
            .context("sending heartbeat")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        response.error_for_status().context("sending heartbeat")?;
        Ok(true)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("southbound refused the registration with {status}: {body}")]
struct RegistrationRejected {
    status: StatusCode,
    body: String,
}

fn spawn_health_server(addr: String) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let app = Router::new().route("/health", get(health));
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("binding spaghetti adapter health listener on {addr}"))?;
        tracing::info!(%addr, "spaghetti adapter health API listening");
        axum::serve(listener, app)
            .await
            .context("spaghetti adapter health server error")?;
        Ok(())
    })
}

async fn health() -> Json<weave_core::Health> {
    Json(weave_core::Health::OK)
}

fn profiles() -> Vec<HopProfile> {
    let srt = TransportClass {
        transport: Transport::Srt,
        roles: RoleSet::both(),
    };
    vec![
        HopProfile {
            id: hops::SENDER_PROFILE.into(),
            ingress: HopEndpointClass::Local(LocalEnd::Input),
            egress: HopEndpointClass::AnyOf(vec![
                LinkClass::Transport(srt.clone()),
                LinkClass::WEBRTC,
            ]),
            max_egresses: Some(hops::MAX_SENDER_EGRESSES),
            media_behavior: MediaBehavior::Unknown,
            merge: false,
            accepts: None,
            timed_delivery: None,
        },
        HopProfile {
            id: hops::WEBRTC_RECEIVER_PROFILE.into(),
            ingress: HopEndpointClass::WEBRTC,
            egress: HopEndpointClass::Local(LocalEnd::Output),
            max_egresses: Some(1),
            media_behavior: MediaBehavior::Unknown,
            merge: false,
            accepts: Some(FormatConstraint {
                container: Some(vec![Container::Rtp]),
                video: None,
                audio: Some(AudioConstraint {
                    codec: Some(vec![AudioCodec::Opus]),
                    sample_rate: None,
                    channels: None,
                }),
            }),
            timed_delivery: None,
        },
        HopProfile {
            id: hops::RECEIVER_PROFILE.into(),
            ingress: HopEndpointClass::Transport(srt),
            egress: HopEndpointClass::Local(LocalEnd::Output),
            max_egresses: Some(1),
            media_behavior: MediaBehavior::Unknown,
            merge: false,
            accepts: Some(FormatConstraint {
                container: Some(vec![Container::MpegTs]),
                video: None,
                audio: Some(AudioConstraint {
                    codec: Some(vec![AudioCodec::Aac]),
                    sample_rate: None,
                    channels: None,
                }),
            }),
            timed_delivery: None,
        },
    ]
}

fn descriptor(config: &AdapterConfig, inventory: &Inventory, status: NodeStatus) -> DeclaredNode {
    NodeDescriptor {
        id: config.node.id.clone(),
        endpoint: config.node.public_endpoint(),
        status,
        capabilities: NodeCapabilities {
            adapters: vec![AdapterDescriptor {
                name: "spaghetti".into(),
                kind: AdapterKind::Spaghetti,
            }],
            hop_profiles: profiles(),
        },
        topology: config.node.topology.clone(),
        inputs: inventory.inputs.clone(),
        outputs: inventory.outputs.clone(),
    }
}

struct Observation {
    inventory: Inventory,
    sources: Vec<SourceInfo>,
    routes: Vec<RouteInfo>,
}

async fn observe(spaghetti: &SpaghettiClient, home: &str) -> Result<Observation, SpaghettiError> {
    let apps = spaghetti.apps().await?;
    let devices = spaghetti.system_devices().await?;
    Ok(Observation {
        inventory: Inventory::detect(&apps, &devices, home),
        sources: spaghetti.sources().await?,
        routes: spaghetti.routes().await?,
    })
}

struct Node {
    registered: bool,
    version_checked: bool,
    inventory: Inventory,
    desired: Vec<DesiredHop>,
    retries: Retries,
    rates: Rates,
    refusals: Refusals,
    exchange_failing: bool,
    statuses: Vec<HopStatus>,
}

async fn sync_loop(
    southbound: &Southbound,
    spaghetti: &SpaghettiClient,
    config: &AdapterConfig,
    reach: Option<&Reach>,
) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut node = Node {
        registered: false,
        version_checked: false,
        inventory: Inventory::default(),
        desired: Vec::new(),
        retries: Retries::default(),
        rates: Rates::default(),
        refusals: Refusals::default(),
        exchange_failing: false,
        statuses: Vec::new(),
    };
    loop {
        match sync_once(southbound, spaghetti, config, reach, &home, &mut node).await {
            Ok(()) => {}
            Err(error)
                if error.is::<RegistrationRejected>()
                    || matches!(
                        error.downcast_ref::<SpaghettiError>(),
                        Some(SpaghettiError::Version { .. })
                    ) =>
            {
                tracing::error!(error = %ErrorChain(error.as_ref()), "spaghetti adapter cannot run; exiting");
                return Err(error);
            }
            Err(error) => {
                tracing::warn!(error = %ErrorChain(error.as_ref()), "spaghetti adapter sync failed");
            }
        }
        tokio::time::sleep(config.spaghetti.poll_interval()).await;
    }
}

async fn sync_once(
    southbound: &Southbound,
    spaghetti: &SpaghettiClient,
    config: &AdapterConfig,
    reach: Option<&Reach>,
    home: &str,
    node: &mut Node,
) -> Result<()> {
    if !node.version_checked {
        match spaghetti.check_version().await {
            Ok(_) => node.version_checked = true,
            Err(error @ SpaghettiError::Version { .. }) => return Err(error.into()),
            Err(error) => tracing::warn!(error = %error, "spaghetti daemon is not answering"),
        }
    }
    let observation = match observe(spaghetti, home).await {
        Ok(observation) => Some(observation),
        Err(error) => {
            tracing::warn!(error = %error, "cannot read the spaghetti daemon");
            node.version_checked = false;
            None
        }
    };
    let status = if observation.is_some() {
        NodeStatus::Ready
    } else {
        NodeStatus::Degraded
    };
    if let Some(observation) = &observation {
        node.inventory = observation.inventory.clone();
    }
    let network_observation = reach.map(Reach::latest);

    if !node.registered {
        southbound
            .register(&NodeRegistration {
                protocol_version: PROTOCOL_VERSION,
                node: descriptor(config, &node.inventory, status),
                observation: network_observation.clone(),
                endpoints: Vec::new(),
                hop_status: node.statuses.clone(),
            })
            .await?;
        node.registered = true;
        tracing::info!(
            node_id = %config.node.id,
            inputs = node.inventory.inputs.len(),
            outputs = node.inventory.outputs.len(),
            "registered with southbound"
        );
    }

    if let Some(desired) = southbound.desired(&config.node.id).await? {
        node.desired = desired;
    }

    let plans: BTreeMap<String, Result<Plan, String>> = node
        .desired
        .iter()
        .map(|hop| (hop.id.clone(), hops::plan(hop, &node.inventory)))
        .collect();
    for (hop_id, plan) in &plans {
        if let Err(reason) = plan {
            tracing::debug!(hop_id, reason, "cannot run desired hop");
        }
    }
    node.retries.retain(&plans);
    let built = match observation {
        Some(observation) => {
            let actions = hops::actions(
                &config.node.id,
                &plans,
                &observation.sources,
                &observation.routes,
            );
            apply(
                spaghetti,
                &config.node.id,
                actions,
                &plans,
                &mut node.retries,
                Instant::now(),
            )
            .await;
            Some((spaghetti.sources().await?, spaghetti.routes().await?))
        }
        None => None,
    };
    node.refusals.retain(&node.desired);
    if let Some((sources, routes)) = &built {
        let peers = hops::built_peers(&config.node.id, sources, routes);
        if !peers.is_empty() {
            let exchanged = signal(
                southbound,
                spaghetti,
                &config.node.id,
                &peers,
                &mut node.refusals,
            )
            .await;
            match (&exchanged, node.exchange_failing) {
                (Err(error), false) => {
                    tracing::warn!(error = %ErrorChain(error.as_ref()), "cannot exchange webrtc descriptions; retrying every poll");
                }
                (Ok(()), true) => tracing::info!("exchanging webrtc descriptions again"),
                _ => {}
            }
            node.exchange_failing = exchanged.is_err();
        }
    }
    let now = Instant::now();
    let (applied, _) = match &built {
        Some((sources, routes)) => hops::applied(&config.node.id, sources, routes),
        None => Default::default(),
    };
    node.statuses = node
        .desired
        .iter()
        .map(|hop| {
            let plan = &plans[&hop.id];
            let built = applied.get(hop.id.as_str());
            if let (Ok(plan), Some(built)) = (plan, built) {
                node.retries.settle(&hop.id, plan, built);
            }
            hops::hop_status(
                hop,
                plan,
                built,
                &node.retries,
                &mut node.rates,
                &node.refusals,
                now,
            )
        })
        .collect();
    let hop_ids: Vec<&str> = node.desired.iter().map(|hop| hop.id.as_str()).collect();
    node.rates.retain(&hop_ids);

    let heartbeat = NodeHeartbeat {
        node_id: config.node.id.clone(),
        status,
        inputs: node.inventory.inputs.clone(),
        outputs: node.inventory.outputs.clone(),
        observation: network_observation,
        endpoints: Vec::new(),
        hop_status: node.statuses.clone(),
    };
    if !southbound.heartbeat(&heartbeat).await? {
        node.registered = false;
    }
    Ok(())
}

async fn signal(
    southbound: &Southbound,
    spaghetti: &SpaghettiClient,
    node_id: &str,
    peers: &[hops::BuiltPeer],
    refusals: &mut Refusals,
) -> Result<()> {
    let local = peers
        .iter()
        .filter_map(|peer| {
            Some(LinkDescription {
                link: peer.attempt.link.clone(),
                generation: peer.attempt.generation,
                description: SessionDescription::new(peer.local_description.clone()?),
            })
        })
        .collect();
    let reply = southbound
        .exchange(node_id, &SignallingExchange { local })
        .await?;
    for attempt in reply.refused {
        let (link, generation) = (attempt.link.clone(), attempt.generation);
        if refusals.refuse(attempt) {
            tracing::warn!(%link, generation, "the controller refused this node's description; reporting the link lost");
        }
    }
    for remote in reply.remote {
        let Some(peer) = peers.iter().find(|peer| {
            peer.attempt.link == remote.link && peer.attempt.generation == remote.generation
        }) else {
            continue;
        };
        match spaghetti
            .set_remote_description(peer.at, remote.generation, remote.description.expose())
            .await
        {
            Ok(()) => {}
            Err(SpaghettiError::Refused(message)) => {
                if refusals.refuse(peer.attempt.clone()) {
                    tracing::warn!(link = %remote.link, generation = remote.generation, message, "spaghetti refused the other end's description; reporting the link lost");
                }
            }
            Err(error) => {
                tracing::warn!(link = %remote.link, generation = remote.generation, error = %error, "cannot apply the other end's description");
            }
        }
    }
    Ok(())
}

async fn apply(
    spaghetti: &SpaghettiClient,
    node_id: &str,
    actions: Vec<Action>,
    plans: &BTreeMap<String, Result<Plan, String>>,
    retries: &mut Retries,
    now: Instant,
) {
    let mut failed = BTreeSet::new();
    for action in actions {
        match action {
            Action::Destroy { source_id } => match spaghetti.destroy_source(source_id).await {
                Ok(()) => tracing::info!(source_id, "removed spaghetti source"),
                Err(error) => {
                    tracing::warn!(source_id, error = %error, "cannot remove a spaghetti source");
                }
            },
            Action::AddRoute {
                hop_id,
                source_id,
                branch_id,
                destination,
            } => {
                add_route(
                    spaghetti,
                    retries,
                    (&hop_id, source_id),
                    (&branch_id, &destination),
                    now,
                )
                .await;
            }
            Action::Source { hop_id, change } => {
                let Some(Ok(plan)) = plans.get(&hop_id) else {
                    continue;
                };
                if failed.contains(&hop_id) || !retries.source_due(&hop_id, plan, now) {
                    continue;
                }
                match change_source(spaghetti, node_id, &hop_id, change).await {
                    Ok(Some(source_id)) => {
                        for (branch_id, destination) in &plan.routes {
                            add_route(
                                spaghetti,
                                retries,
                                (&hop_id, source_id),
                                (branch_id, destination),
                                now,
                            )
                            .await;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(hop_id, error = %error, "cannot apply hop");
                        retries.source_failed(&hop_id, plan, now);
                        failed.insert(hop_id);
                    }
                }
            }
        }
    }
}

async fn change_source(
    spaghetti: &SpaghettiClient,
    node_id: &str,
    hop_id: &str,
    change: SourceChange,
) -> Result<Option<u32>, SpaghettiError> {
    match change {
        SourceChange::Build(source) => {
            let source_id = spaghetti
                .create_source(&source, &hops::tag(node_id, hop_id))
                .await?;
            tracing::info!(hop_id, source_id, "built hop");
            Ok(Some(source_id))
        }
        SourceChange::Retap { source_id, pids } => {
            spaghetti.set_tap_processes(source_id, &pids).await?;
            tracing::info!(hop_id, ?pids, "retapped input");
            Ok(None)
        }
        SourceChange::RemoveRoute { route_id } => {
            spaghetti.destroy_route(route_id).await?;
            tracing::info!(hop_id, route_id, "removed route");
            Ok(None)
        }
    }
}

async fn add_route(
    spaghetti: &SpaghettiClient,
    retries: &mut Retries,
    (hop_id, source_id): (&str, u32),
    (branch_id, destination): (&str, &Destination),
    now: Instant,
) {
    if !retries.route_due(hop_id, branch_id, destination, now) {
        return;
    }
    match spaghetti.create_route(source_id, destination, 1.0).await {
        Ok(route_id) => tracing::info!(hop_id, branch_id, route_id, "added route"),
        Err(error) => {
            tracing::warn!(hop_id, branch_id, error = %error, "cannot add route");
            retries.route_failed(hop_id, branch_id, destination, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use serde_json::json;
    use spaghetti_common::ipc_protocol::{
        AppInfo, PeerRole, SourceSpec, SrtMode, SrtSocket, TapMode, WebRtcPeer,
    };

    use super::*;

    #[tokio::test]
    async fn a_refused_route_neither_tears_down_its_source_nor_holds_back_the_next() {
        let webrtc = Destination::WebRtc(WebRtcPeer {
            link: "weave-music-receiver-anna".into(),
            generation: 7,
            role: PeerRole::Offer,
            stun: Vec::new(),
            turn: Vec::new(),
        });
        let srt = Destination::Srt(SrtSocket {
            mode: SrtMode::Call {
                host: "192.168.1.30".into(),
                port: 23000,
            },
            latency_ms: 120,
            passphrase: None,
            key_size: None,
        });
        let plan = Plan {
            source: SourceSpec::Tap {
                pids: vec![731],
                mode: TapMode::Mirror,
            },
            routes: vec![
                ("anna".into(), webrtc.clone()),
                ("filip".into(), srt.clone()),
            ],
        };
        let hop_id = "weave-music-sender";
        let plans = BTreeMap::from([(hop_id.to_string(), Ok(plan.clone()))]);
        let (spaghetti, daemon) = daemon::fake::daemon(vec![
            json!({ "SourceCreated": { "source_id": 4 } }),
            json!({ "Error": { "message": "the network is changing" } }),
            json!({ "RouteCreated": { "route_id": 41 } }),
        ]);
        let mut retries = Retries::default();
        let build = Action::Source {
            hop_id: hop_id.into(),
            change: SourceChange::Build(plan.source.clone()),
        };
        apply(
            &spaghetti,
            "alfreds-mac",
            vec![build],
            &plans,
            &mut retries,
            Instant::now(),
        )
        .await;

        let requests = tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .unwrap()
            .unwrap();
        assert!(requests[0]["CreateSource"].is_object());
        assert!(requests[1]["CreateRoute"]["destination"]["WebRtc"].is_object());
        assert!(requests[2]["CreateRoute"]["destination"]["Srt"].is_object());
        assert!(!retries.source_failing(hop_id, &plan));
        assert!(retries.route_failing(hop_id, "anna", &webrtc));
        assert!(!retries.route_failing(hop_id, "filip", &srt));
    }

    #[test]
    fn help_hides_every_env_value() {
        let command = Args::command();
        let shown: Vec<String> = command
            .get_arguments()
            .filter(|arg| arg.get_env().is_some() && !arg.is_hide_env_values_set())
            .map(|arg| arg.get_id().to_string())
            .collect();
        assert!(
            shown.is_empty(),
            "--help prints the environment value of {shown:?}"
        );
    }

    #[test]
    fn the_registration_is_valid_for_the_controller() {
        let config: AdapterConfig = serde_norway::from_str(
            r"
node:
  id: alfreds-mac
  southbound_url: http://127.0.0.1:8081
  listen: 127.0.0.1:8092
  topology:
    site:
      id: lan
      listeners:
        srt:
          port_range: { start: 23000, end: 23099 }
",
        )
        .unwrap();
        let inventory = Inventory::detect(
            &[AppInfo {
                name: "Music".into(),
                bundle_id: Some("com.apple.Music".into()),
                bundle_path: Some("/System/Applications/Music.app".into()),
                pids: vec![731],
            }],
            &[],
            "/Users/alfred",
        );
        let node = descriptor(&config, &inventory, NodeStatus::Ready);
        assert!(weave_core::validate_node(&node).is_empty());
        assert!(node.capabilities.offers_local(LocalEnd::Input));
        assert!(node.capabilities.offers_local(LocalEnd::Output));
        assert_eq!(node.inputs[0].id, "music");
    }
}
