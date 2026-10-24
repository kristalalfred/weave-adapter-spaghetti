use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use spaghetti_common::ipc_protocol::{
    Destination, FlowState, FlowStatus, PeerRef, PeerRole, RouteInfo, SourceInfo, SourceSpec,
    SrtMode, SrtSocket, TapMode, TurnServer, WebRtcPeer,
};
use weave_core::{
    DesiredHop, EgressStatus, HopState, HopStatus, LinkCondition, LinkStats, LocalEnd, LocalSocket,
    PayloadKind, PeerAttempt, PeerLinkId, PeerReport, PeerSocket, ResolvedAddr, SocketSpec,
    SocketStatus, SrtSocket as WeaveSrt,
};

use crate::inventory::Inventory;

pub const SENDER_PROFILE: &str = "app-audio-out";
pub const RECEIVER_PROFILE: &str = "srt-to-speaker";
pub const WEBRTC_RECEIVER_PROFILE: &str = "webrtc-to-speaker";
pub const MAX_SENDER_EGRESSES: usize = 8;

const TAG_PREFIX: &str = "weave:";
const DEFAULT_LATENCY_MS: u32 = 200;
const RATE_WINDOW_SECS: f64 = 0.2;
const SRT_PASSPHRASE_CHARS: std::ops::RangeInclusive<usize> = 10..=79;
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub source: SourceSpec,
    pub routes: Vec<(String, Destination)>,
}

#[must_use]
pub fn tag(node_id: &str, hop_id: &str) -> String {
    format!("{TAG_PREFIX}{node_id}/{hop_id}")
}

pub fn plan(hop: &DesiredHop, inventory: &Inventory) -> Result<Plan, String> {
    if hop.merge_ingress.is_some() {
        return Err("this node does not merge two paths".into());
    }
    if hop.delivery.is_some() {
        return Err("this node does not run timed delivery".into());
    }
    match hop.profile_id.as_str() {
        SENDER_PROFILE => {
            let SocketSpec::Local(LocalSocket {
                end: LocalEnd::Input,
                id,
            }) = &hop.ingress
            else {
                return Err(format!(
                    "{SENDER_PROFILE} takes an input, not {}",
                    hop.ingress
                ));
            };
            let pids = inventory
                .pids(id)
                .ok_or_else(|| format!("input {id} is not running"))?;
            let routes = hop
                .egresses
                .iter()
                .map(|egress| {
                    let destination = match &egress.socket {
                        SocketSpec::Webrtc(peer) => {
                            Destination::WebRtc(webrtc(peer, PeerRole::Offer))
                        }
                        socket => Destination::Srt(srt(socket)?),
                    };
                    Ok((egress.branch_id.clone(), destination))
                })
                .collect::<Result<_, String>>()?;
            Ok(Plan {
                source: SourceSpec::Tap {
                    pids: pids.to_vec(),
                    mode: TapMode::Mirror,
                },
                routes,
            })
        }
        profile @ (RECEIVER_PROFILE | WEBRTC_RECEIVER_PROFILE) => {
            let source = match (profile, &hop.ingress) {
                (RECEIVER_PROFILE, socket) => SourceSpec::Srt(srt(socket)?),
                (_, SocketSpec::Webrtc(peer)) => SourceSpec::WebRtc(webrtc(peer, PeerRole::Answer)),
                (_, socket) => return Err(format!("{profile} takes a webrtc link, not {socket}")),
            };
            let routes = hop
                .egresses
                .iter()
                .map(|egress| match &egress.socket {
                    SocketSpec::Local(LocalSocket {
                        end: LocalEnd::Output,
                        id,
                    }) => inventory
                        .device(id)
                        .map(|uid| {
                            (
                                egress.branch_id.clone(),
                                Destination::Device {
                                    uid: uid.to_string(),
                                },
                            )
                        })
                        .ok_or_else(|| format!("output {id} is not connected")),
                    other => Err(format!("{profile} feeds an output, not {other}")),
                })
                .collect::<Result<_, String>>()?;
            Ok(Plan { source, routes })
        }
        other => Err(format!("this node offers no hop profile {other}")),
    }
}

fn webrtc(peer: &PeerSocket, role: PeerRole) -> WebRtcPeer {
    WebRtcPeer {
        link: peer.attempt.link.to_string(),
        generation: peer.attempt.generation,
        role,
        stun: peer.stun.clone(),
        turn: peer
            .turn
            .iter()
            .map(|server| TurnServer {
                address: server.address.clone(),
                username: server.username.clone(),
                credential: server.credential.expose().to_string(),
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltPeer {
    pub at: PeerRef,
    pub attempt: PeerAttempt,
    pub local_description: Option<String>,
}

#[must_use]
pub fn built_peers(node_id: &str, sources: &[SourceInfo], routes: &[RouteInfo]) -> Vec<BuiltPeer> {
    let (applied, _) = applied(node_id, sources, routes);
    let peer = |spec: &WebRtcPeer| PeerAttempt {
        link: PeerLinkId::new(spec.link.clone()),
        generation: spec.generation,
    };
    applied
        .values()
        .flat_map(|built| {
            let source = match &built.source.spec {
                SourceSpec::WebRtc(spec) => Some(BuiltPeer {
                    at: PeerRef::Source(built.source.source_id),
                    attempt: peer(spec),
                    local_description: built.source.local_description.clone(),
                }),
                _ => None,
            };
            let routes = built
                .routes
                .iter()
                .filter_map(move |route| match &route.destination {
                    Destination::WebRtc(spec) => Some(BuiltPeer {
                        at: PeerRef::Route(route.route_id),
                        attempt: peer(spec),
                        local_description: route.local_description.clone(),
                    }),
                    _ => None,
                });
            source.into_iter().chain(routes)
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct Refusals(BTreeSet<PeerAttempt>);

impl Refusals {
    pub fn refuse(&mut self, attempt: PeerAttempt) -> bool {
        self.0.insert(attempt)
    }

    pub fn retain(&mut self, desired: &[DesiredHop]) {
        let wanted: BTreeSet<&PeerAttempt> = desired
            .iter()
            .flat_map(DesiredHop::peer_ends)
            .map(|(_, socket)| &socket.attempt)
            .collect();
        self.0.retain(|attempt| wanted.contains(attempt));
    }

    fn contains(&self, attempt: &PeerAttempt) -> bool {
        self.0.contains(attempt)
    }
}

fn srt(socket: &SocketSpec) -> Result<SrtSocket, String> {
    let SocketSpec::Srt(socket) = socket else {
        return Err(format!("expected an srt socket, not {socket}"));
    };
    let params = socket.params();
    if params.payload != PayloadKind::Raw {
        return Err("this node carries only raw SRT payloads".into());
    }
    if let Some(passphrase) = &params.passphrase
        && !SRT_PASSPHRASE_CHARS.contains(&passphrase.expose().len())
    {
        return Err("this node takes SRT passphrases of 10 to 79 characters".into());
    }
    let mode = match socket {
        WeaveSrt::Listen { port, .. } => SrtMode::Listen { port: *port },
        WeaveSrt::Connect { host, port, .. } => SrtMode::Call {
            host: host.clone(),
            port: *port,
        },
    };
    Ok(SrtSocket {
        mode,
        latency_ms: params.latency.unwrap_or(DEFAULT_LATENCY_MS),
        passphrase: params
            .passphrase
            .as_ref()
            .map(|phrase| phrase.expose().to_string()),
        key_size: params.pbkeylen,
    })
}

#[derive(Debug)]
pub struct Applied<'a> {
    pub source: &'a SourceInfo,
    pub routes: Vec<&'a RouteInfo>,
}

impl Applied<'_> {
    fn source_settled(&self, plan: &Plan) -> bool {
        self.source.spec == plan.source && self.unwanted_routes(plan).next().is_none()
    }

    fn unwanted_routes(&self, plan: &Plan) -> impl Iterator<Item = &RouteInfo> {
        self.routes
            .iter()
            .enumerate()
            .filter_map(move |(index, route)| {
                let planned = plan
                    .routes
                    .iter()
                    .any(|(_, destination)| destination == &route.destination);
                let repeated = self.routes[..index]
                    .iter()
                    .any(|earlier| earlier.destination == route.destination);
                (!planned || repeated).then_some(*route)
            })
    }

    fn route(&self, destination: &Destination) -> Option<&RouteInfo> {
        self.routes
            .iter()
            .copied()
            .find(|route| &route.destination == destination)
    }
}

#[must_use]
pub fn applied<'a>(
    node_id: &str,
    sources: &'a [SourceInfo],
    routes: &'a [RouteInfo],
) -> (BTreeMap<&'a str, Applied<'a>>, Vec<u32>) {
    let prefix = tag(node_id, "");
    let mut by_hop = BTreeMap::new();
    let mut duplicates = Vec::new();
    for source in sources {
        let Some(hop_id) = source
            .tag
            .as_deref()
            .and_then(|tag| tag.strip_prefix(prefix.as_str()))
        else {
            continue;
        };
        if by_hop.contains_key(hop_id) {
            duplicates.push(source.source_id);
            continue;
        }
        by_hop.insert(
            hop_id,
            Applied {
                source,
                routes: routes
                    .iter()
                    .filter(|route| route.source_id == source.source_id)
                    .collect(),
            },
        );
    }
    (by_hop, duplicates)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Destroy {
        source_id: u32,
    },
    Source {
        hop_id: String,
        change: SourceChange,
    },
    AddRoute {
        hop_id: String,
        source_id: u32,
        branch_id: String,
        destination: Destination,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SourceChange {
    Build(SourceSpec),
    Retap { source_id: u32, pids: Vec<u32> },
    RemoveRoute { route_id: u32 },
}

fn reusable(built: &SourceSpec, wanted: &SourceSpec) -> bool {
    match (built, wanted) {
        (SourceSpec::Tap { mode: built, .. }, SourceSpec::Tap { mode: wanted, .. }) => {
            built == wanted
        }
        (built, wanted) => built == wanted,
    }
}

#[must_use]
pub fn actions(
    node_id: &str,
    plans: &BTreeMap<String, Result<Plan, String>>,
    sources: &[SourceInfo],
    routes: &[RouteInfo],
) -> Vec<Action> {
    let (applied, duplicates) = applied(node_id, sources, routes);
    let mut actions: Vec<Action> = duplicates
        .into_iter()
        .map(|source_id| Action::Destroy { source_id })
        .collect();
    let mut updates = Vec::new();
    for (hop_id, built) in &applied {
        let source_id = built.source.source_id;
        let Some(Ok(plan)) = plans.get(*hop_id) else {
            actions.push(Action::Destroy { source_id });
            continue;
        };
        if !reusable(&built.source.spec, &plan.source) {
            actions.push(Action::Destroy { source_id });
            continue;
        }
        if let (SourceSpec::Tap { pids: tapped, .. }, SourceSpec::Tap { pids, .. }) =
            (&built.source.spec, &plan.source)
            && tapped != pids
        {
            updates.push(Action::Source {
                hop_id: (*hop_id).to_string(),
                change: SourceChange::Retap {
                    source_id,
                    pids: pids.clone(),
                },
            });
        }
        for route in built.unwanted_routes(plan) {
            actions.push(Action::Source {
                hop_id: (*hop_id).to_string(),
                change: SourceChange::RemoveRoute {
                    route_id: route.route_id,
                },
            });
        }
        for (branch_id, destination) in &plan.routes {
            if built.route(destination).is_none() {
                updates.push(Action::AddRoute {
                    hop_id: (*hop_id).to_string(),
                    source_id,
                    branch_id: branch_id.clone(),
                    destination: destination.clone(),
                });
            }
        }
    }
    actions.extend(updates);
    for (hop_id, plan) in plans {
        let Ok(plan) = plan else { continue };
        let kept = applied
            .get(hop_id.as_str())
            .is_some_and(|built| reusable(&built.source.spec, &plan.source));
        if !kept {
            actions.push(Action::Source {
                hop_id: hop_id.clone(),
                change: SourceChange::Build(plan.source.clone()),
            });
        }
    }
    actions
}

#[derive(Debug)]
struct Retry<T> {
    wanted: T,
    at: Instant,
    delay: Duration,
}

impl<T: PartialEq + Clone> Retry<T> {
    fn due(retry: Option<&Self>, wanted: &T, now: Instant) -> bool {
        retry.is_none_or(|retry| &retry.wanted != wanted || now >= retry.at)
    }

    fn failing(retry: Option<&Self>, wanted: &T) -> bool {
        retry.is_some_and(|retry| &retry.wanted == wanted)
    }

    fn after(retry: Option<&Self>, wanted: &T, now: Instant) -> Self {
        let delay = match retry {
            Some(retry) if &retry.wanted == wanted => (retry.delay * 2).min(LAST_RETRY),
            _ => FIRST_RETRY,
        };
        Self {
            wanted: wanted.clone(),
            at: now + delay,
            delay,
        }
    }
}

#[derive(Debug, Default)]
pub struct Retries {
    sources: BTreeMap<String, Retry<Plan>>,
    routes: BTreeMap<String, BTreeMap<String, Retry<Destination>>>,
}

impl Retries {
    #[must_use]
    pub fn source_due(&self, hop_id: &str, plan: &Plan, now: Instant) -> bool {
        Retry::due(self.sources.get(hop_id), plan, now)
    }

    #[must_use]
    pub fn source_failing(&self, hop_id: &str, plan: &Plan) -> bool {
        Retry::failing(self.sources.get(hop_id), plan)
    }

    pub fn source_failed(&mut self, hop_id: &str, plan: &Plan, now: Instant) {
        let retry = Retry::after(self.sources.get(hop_id), plan, now);
        self.sources.insert(hop_id.to_string(), retry);
    }

    fn route(&self, hop_id: &str, branch_id: &str) -> Option<&Retry<Destination>> {
        self.routes.get(hop_id)?.get(branch_id)
    }

    #[must_use]
    pub fn route_due(
        &self,
        hop_id: &str,
        branch_id: &str,
        destination: &Destination,
        now: Instant,
    ) -> bool {
        Retry::due(self.route(hop_id, branch_id), destination, now)
    }

    #[must_use]
    pub fn route_failing(&self, hop_id: &str, branch_id: &str, destination: &Destination) -> bool {
        Retry::failing(self.route(hop_id, branch_id), destination)
    }

    pub fn route_failed(
        &mut self,
        hop_id: &str,
        branch_id: &str,
        destination: &Destination,
        now: Instant,
    ) {
        let retry = Retry::after(self.route(hop_id, branch_id), destination, now);
        self.routes
            .entry(hop_id.to_string())
            .or_default()
            .insert(branch_id.to_string(), retry);
    }

    pub fn settle(&mut self, hop_id: &str, plan: &Plan, built: &Applied) {
        if built.source_settled(plan) {
            self.sources.remove(hop_id);
        }
        if let Some(routes) = self.routes.get_mut(hop_id) {
            routes.retain(|_, retry| built.route(&retry.wanted).is_none());
        }
    }

    pub fn retain(&mut self, plans: &BTreeMap<String, Result<Plan, String>>) {
        self.sources.retain(
            |hop_id, retry| matches!(plans.get(hop_id), Some(Ok(plan)) if plan == &retry.wanted),
        );
        self.routes.retain(|hop_id, routes| {
            let Some(Ok(plan)) = plans.get(hop_id) else {
                return false;
            };
            routes.retain(|branch_id, retry| {
                plan.routes.iter().any(|(branch, destination)| {
                    branch == branch_id && destination == &retry.wanted
                })
            });
            !routes.is_empty()
        });
    }
}

#[derive(Debug, Default)]
pub struct Rates(BTreeMap<String, (u64, Instant, f64)>);

impl Rates {
    pub fn sample(&mut self, key: String, bytes: u64, now: Instant) -> f64 {
        match self.0.get_mut(&key) {
            Some((last_bytes, at, rate)) => {
                let elapsed = now.duration_since(*at).as_secs_f64();
                if bytes < *last_bytes {
                    *last_bytes = bytes;
                    *at = now;
                    *rate = 0.0;
                } else if elapsed >= RATE_WINDOW_SECS {
                    *rate = (bytes - *last_bytes) as f64 * 8.0 / elapsed / 1_000_000.0;
                    *last_bytes = bytes;
                    *at = now;
                }
                *rate
            }
            None => {
                self.0.insert(key, (bytes, now, 0.0));
                0.0
            }
        }
    }

    pub fn retain(&mut self, hop_ids: &[&str]) {
        self.0
            .retain(|key, _| hop_ids.iter().any(|id| key.starts_with(&format!("{id}/"))));
    }
}

#[must_use]
pub fn hop_status(
    hop: &DesiredHop,
    plan: &Result<Plan, String>,
    built: Option<&Applied>,
    retries: &Retries,
    rates: &mut Rates,
    refusals: &Refusals,
    now: Instant,
) -> HopStatus {
    let running = match (plan, built) {
        (Ok(plan), Some(built)) if built.source.spec == plan.source => Some((plan, built)),
        _ => None,
    };
    let state = match (plan, running) {
        (Err(_), _) => HopState::Failed,
        (Ok(plan), _) if retries.source_failing(&hop.id, plan) => HopState::Failed,
        (Ok(_), Some((plan, built)))
            if built.source_settled(plan)
                && plan.routes.iter().all(|(branch_id, destination)| {
                    built.route(destination).is_some()
                        || retries.route_failing(&hop.id, branch_id, destination)
                }) =>
        {
            HopState::Provisioned
        }
        (Ok(_), _) => HopState::Pending,
    };
    let ingress = match running {
        Some((_, built)) => socket_status(
            &built.source.status,
            &hop.ingress,
            rates.sample(
                format!("{}/ingress", hop.id),
                built.source.status.bytes,
                now,
            ),
            Direction::Receiving,
            refusals,
        ),
        None => idle(&hop.ingress),
    };
    let egresses = hop
        .egresses
        .iter()
        .map(|egress| {
            let status = running
                .and_then(|(plan, built)| {
                    let (_, destination) = plan
                        .routes
                        .iter()
                        .find(|(branch, _)| branch == &egress.branch_id)?;
                    Some(match built.route(destination) {
                        Some(route) => socket_status(
                            &route.status,
                            &egress.socket,
                            rates.sample(
                                format!("{}/egress/{}", hop.id, egress.branch_id),
                                route.status.bytes,
                                now,
                            ),
                            Direction::Sending,
                            refusals,
                        ),
                        None if retries.route_failing(&hop.id, &egress.branch_id, destination) => {
                            failed_route(&egress.socket)
                        }
                        None => idle(&egress.socket),
                    })
                })
                .unwrap_or_else(|| idle(&egress.socket));
            EgressStatus {
                branch_id: egress.branch_id.clone(),
                status,
            }
        })
        .collect();
    HopStatus {
        id: hop.id.clone(),
        node_id: hop.node_id.clone(),
        revision: hop.revision(),
        state,
        ingress,
        merge_ingress: None,
        egresses,
        delivery: None,
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Sending,
    Receiving,
}

fn resolved(socket: &SocketSpec) -> Option<ResolvedAddr> {
    match socket {
        SocketSpec::Srt(WeaveSrt::Listen { port, .. }) => Some(ResolvedAddr {
            host: "0.0.0.0".into(),
            port: *port,
        }),
        SocketSpec::Srt(WeaveSrt::Connect { host, port, .. }) => Some(ResolvedAddr {
            host: host.clone(),
            port: *port,
        }),
        _ => None,
    }
}

fn idle(socket: &SocketSpec) -> SocketStatus {
    SocketStatus {
        peer: None,
        condition: LinkCondition::Idle,
        resolved: resolved(socket),
        stats: None,
    }
}

fn failed_route(socket: &SocketSpec) -> SocketStatus {
    SocketStatus {
        peer: match socket {
            SocketSpec::Webrtc(peer) => Some(PeerReport {
                link: peer.attempt.link.clone(),
                generation: peer.attempt.generation,
                relayed: false,
            }),
            _ => None,
        },
        condition: LinkCondition::Lost,
        resolved: resolved(socket),
        stats: None,
    }
}

fn socket_status(
    flow: &FlowStatus,
    socket: &SocketSpec,
    rate_mbps: f64,
    direction: Direction,
    refusals: &Refusals,
) -> SocketStatus {
    let condition = match flow.state {
        FlowState::Idle => LinkCondition::Idle,
        FlowState::Connecting => LinkCondition::Connecting,
        FlowState::Connected => LinkCondition::Connected,
        FlowState::Flowing => LinkCondition::Flowing,
        FlowState::Stalled => LinkCondition::Stalled,
        FlowState::Unreachable => LinkCondition::Unreachable,
        FlowState::Lost => LinkCondition::Lost,
    };
    let lost = i64::try_from(flow.packets_lost).unwrap_or(i64::MAX);
    let retransmitted = i64::try_from(flow.packets_retransmitted).unwrap_or(i64::MAX);
    let mut stats = LinkStats {
        connections: usize::from(flow.peer.is_some()),
        rate_mbps,
        ..LinkStats::default()
    };
    match direction {
        Direction::Sending => {
            stats.packets_sent_lost = lost;
            stats.packets_retransmitted = retransmitted;
        }
        Direction::Receiving => {
            stats.packets_received_lost = lost;
            stats.packets_received_retransmitted = retransmitted;
        }
    }
    match socket {
        SocketSpec::Webrtc(peer) => SocketStatus {
            peer: Some(PeerReport {
                link: peer.attempt.link.clone(),
                generation: peer.attempt.generation,
                relayed: flow.relayed,
            }),
            condition: if refusals.contains(&peer.attempt) && !condition.is_terminal() {
                LinkCondition::Lost
            } else {
                condition
            },
            resolved: flow
                .peer
                .as_deref()
                .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
                .map(|address| ResolvedAddr {
                    host: address.ip().to_string(),
                    port: address.port(),
                }),
            stats: Some(stats),
        },
        socket => SocketStatus {
            peer: None,
            condition,
            resolved: resolved(socket),
            stats: Some(stats),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spaghetti_common::ipc_protocol::{AppInfo, SystemDeviceInfo};
    use weave_core::{DesiredEgress, HopRole, Passphrase, SrtParams, Track};

    fn inventory() -> Inventory {
        Inventory::detect(
            &[AppInfo {
                name: "Music".into(),
                bundle_id: Some("com.apple.Music".into()),
                bundle_path: Some("/System/Applications/Music.app".into()),
                pids: vec![731],
            }],
            &[SystemDeviceInfo {
                uid: "BuiltInSpeakerDevice".into(),
                name: "MacBook Pro Speakers".into(),
                channels: 2,
                is_output: true,
            }],
            "/Users/alfred",
        )
    }

    fn srt_connect(host: &str, port: u16) -> SocketSpec {
        SocketSpec::Srt(WeaveSrt::Connect {
            host: host.into(),
            port,
            params: SrtParams {
                latency: Some(120),
                passphrase: Some(Passphrase::new("controller-derived-key")),
                pbkeylen: Some(32),
                ..SrtParams::default()
            },
        })
    }

    fn sender() -> DesiredHop {
        DesiredHop {
            id: "weave-music-sender".into(),
            node_id: "alfreds-mac".into(),
            profile_id: SENDER_PROFILE.into(),
            role: HopRole::Sender,
            ingress: SocketSpec::input("music"),
            merge_ingress: None,
            egresses: vec![DesiredEgress {
                branch_id: "filip".into(),
                socket: srt_connect("192.168.1.30", 23000),
            }],
            tracks: Some(vec![Track::Audio]),
            delivery: None,
        }
    }

    fn receiver() -> DesiredHop {
        DesiredHop {
            id: "weave-music-receiver-filip".into(),
            node_id: "filips-mac".into(),
            profile_id: RECEIVER_PROFILE.into(),
            role: HopRole::Receiver,
            ingress: SocketSpec::srt_listen(23000, 120),
            merge_ingress: None,
            egresses: vec![DesiredEgress {
                branch_id: "filip".into(),
                socket: SocketSpec::output("macbook-pro-speakers"),
            }],
            tracks: Some(vec![Track::Audio]),
            delivery: None,
        }
    }

    fn flowing(bytes: u64) -> FlowStatus {
        FlowStatus {
            state: FlowState::Flowing,
            bytes,
            peer: Some("192.168.1.30:23000".into()),
            relayed: false,
            packets_lost: 2,
            packets_retransmitted: 5,
        }
    }

    fn webrtc_socket(generation: u32) -> SocketSpec {
        SocketSpec::Webrtc(PeerSocket {
            attempt: PeerAttempt {
                link: PeerLinkId::new("weave-music-receiver-filip"),
                generation,
            },
            stun: vec!["stun.example:3478".into()],
            turn: vec![weave_core::TurnServer {
                address: "turn.example:3478".into(),
                username: "1761386400:weave-music-receiver-filip".into(),
                credential: weave_core::TurnCredential::new("c2VjcmV0"),
            }],
        })
    }

    fn webrtc_peer(generation: u32, role: PeerRole) -> WebRtcPeer {
        WebRtcPeer {
            link: "weave-music-receiver-filip".into(),
            generation,
            role,
            stun: vec!["stun.example:3478".into()],
            turn: vec![TurnServer {
                address: "turn.example:3478".into(),
                username: "1761386400:weave-music-receiver-filip".into(),
                credential: "c2VjcmV0".into(),
            }],
        }
    }

    fn webrtc_receiver(generation: u32) -> DesiredHop {
        DesiredHop {
            profile_id: WEBRTC_RECEIVER_PROFILE.into(),
            ingress: webrtc_socket(generation),
            ..receiver()
        }
    }

    #[test]
    fn a_sender_offers_its_webrtc_egress_next_to_an_srt_one() {
        let mut hop = sender();
        hop.egresses.push(DesiredEgress {
            branch_id: "anna".into(),
            socket: webrtc_socket(7),
        });
        let plan = plan(&hop, &inventory()).unwrap();
        assert_eq!(plan.routes.len(), 2);
        assert!(matches!(plan.routes[0].1, Destination::Srt(_)));
        assert_eq!(
            plan.routes[1],
            (
                "anna".to_string(),
                Destination::WebRtc(webrtc_peer(7, PeerRole::Offer))
            )
        );
    }

    #[test]
    fn a_webrtc_receiver_answers_and_each_receiver_profile_takes_only_its_own_link() {
        let plan = plan(&webrtc_receiver(7), &inventory()).unwrap();
        assert_eq!(
            plan.source,
            SourceSpec::WebRtc(webrtc_peer(7, PeerRole::Answer))
        );
        let srt_on_webrtc = DesiredHop {
            profile_id: WEBRTC_RECEIVER_PROFILE.into(),
            ..receiver()
        };
        assert!(super::plan(&srt_on_webrtc, &inventory()).is_err());
        let webrtc_on_srt = DesiredHop {
            profile_id: RECEIVER_PROFILE.into(),
            ..webrtc_receiver(7)
        };
        assert!(super::plan(&webrtc_on_srt, &inventory()).is_err());
    }

    #[test]
    fn a_webrtc_end_reports_its_attempt_path_and_a_refusal_as_lost_until_the_generation_moves() {
        let hop = webrtc_receiver(7);
        let wanted = plan(&hop, &inventory()).unwrap();
        let (mut source, routes) = built(&hop, &wanted, 1, 1_000);
        source.status.peer = Some("91.197.40.98:30025".into());
        source.status.relayed = true;
        let status = |refusals: &Refusals, source: &SourceInfo| {
            let (current, _) = applied(&hop.node_id, std::slice::from_ref(source), &routes);
            hop_status(
                &hop,
                &Ok(wanted.clone()),
                current.get(hop.id.as_str()),
                &Retries::default(),
                &mut Rates::default(),
                refusals,
                Instant::now(),
            )
            .ingress
        };
        let mut refusals = Refusals::default();
        let reported = status(&refusals, &source);
        assert_eq!(reported.condition, LinkCondition::Flowing);
        assert_eq!(
            reported.peer,
            Some(PeerReport {
                link: PeerLinkId::new("weave-music-receiver-filip"),
                generation: 7,
                relayed: true,
            })
        );
        assert_eq!(
            reported.resolved,
            Some(ResolvedAddr {
                host: "91.197.40.98".into(),
                port: 30025
            })
        );

        refusals.refuse(PeerAttempt {
            link: PeerLinkId::new("weave-music-receiver-filip"),
            generation: 7,
        });
        refusals.retain(std::slice::from_ref(&hop));
        assert_eq!(status(&refusals, &source).condition, LinkCondition::Lost);
        source.status.state = FlowState::Unreachable;
        assert_eq!(
            status(&refusals, &source).condition,
            LinkCondition::Unreachable
        );
        source.status.state = FlowState::Connecting;
        refusals.retain(&[webrtc_receiver(8)]);
        assert_eq!(
            status(&refusals, &source).condition,
            LinkCondition::Connecting
        );
    }

    fn built(
        hop: &DesiredHop,
        plan: &Plan,
        source_id: u32,
        bytes: u64,
    ) -> (SourceInfo, Vec<RouteInfo>) {
        let source = SourceInfo {
            source_id,
            spec: plan.source.clone(),
            tag: Some(tag(&hop.node_id, &hop.id)),
            label: "x".into(),
            status: flowing(bytes),
            local_description: None,
        };
        let routes = plan
            .routes
            .iter()
            .enumerate()
            .map(|(n, (_, destination))| RouteInfo {
                route_id: source_id * 10 + n as u32,
                source_id,
                destination: destination.clone(),
                volume: 1.0,
                status: flowing(bytes),
                local_description: None,
            })
            .collect();
        (source, routes)
    }

    #[test]
    fn a_sender_taps_the_inputs_app_and_calls_each_egress_over_srt() {
        let plan = plan(&sender(), &inventory()).unwrap();
        assert_eq!(
            plan,
            Plan {
                source: SourceSpec::Tap {
                    pids: vec![731],
                    mode: TapMode::Mirror
                },
                routes: vec![(
                    "filip".into(),
                    Destination::Srt(SrtSocket {
                        mode: SrtMode::Call {
                            host: "192.168.1.30".into(),
                            port: 23000
                        },
                        latency_ms: 120,
                        passphrase: Some("controller-derived-key".into()),
                        key_size: Some(32),
                    })
                )],
            }
        );
    }

    #[test]
    fn a_receiver_listens_over_srt_and_plays_on_the_outputs_device() {
        let plan = plan(&receiver(), &inventory()).unwrap();
        assert_eq!(
            plan.source,
            SourceSpec::Srt(SrtSocket {
                mode: SrtMode::Listen { port: 23000 },
                latency_ms: 120,
                passphrase: None,
                key_size: None,
            })
        );
        assert_eq!(
            plan.routes,
            [(
                "filip".to_string(),
                Destination::Device {
                    uid: "BuiltInSpeakerDevice".into()
                }
            )]
        );
    }

    #[test]
    fn hops_this_node_cannot_run_are_refused() {
        let mut gone = sender();
        gone.ingress = SocketSpec::input("spotify");
        assert_eq!(
            plan(&gone, &inventory()),
            Err("input spotify is not running".into())
        );
        let mut foreign = sender();
        foreign.profile_id = "srt-forward".into();
        assert!(plan(&foreign, &inventory()).is_err());
        let mut timed = receiver();
        timed.ingress = SocketSpec::Srt(WeaveSrt::Listen {
            port: 23000,
            params: SrtParams {
                payload: PayloadKind::WeaveTimedV1,
                ..SrtParams::default()
            },
        });
        assert!(plan(&timed, &inventory()).is_err());
    }

    fn sorted(mut actions: Vec<Action>) -> Vec<Action> {
        actions.sort_by_key(|action| format!("{action:?}"));
        actions
    }

    #[test]
    fn actions_build_what_is_missing_and_destroy_only_what_is_unwanted() {
        let hop = sender();
        let wanted = plan(&hop, &inventory()).unwrap();
        let (current, current_routes) = built(&hop, &wanted, 1, 0);
        let missing = DesiredHop {
            id: "weave-music-2-sender".into(),
            ..sender()
        };
        let (unwanted, _) = built(
            &DesiredHop {
                id: "weave-old-sender".into(),
                ..sender()
            },
            &wanted,
            3,
            0,
        );
        let untagged = SourceInfo {
            tag: None,
            ..built(&hop, &wanted, 4, 0).0
        };
        let (other_node, _) = built(&receiver(), &wanted, 5, 0);
        let sources = [current, unwanted, untagged, other_node];
        let plans = BTreeMap::from([
            (hop.id.clone(), Ok(wanted.clone())),
            (missing.id.clone(), Ok(wanted.clone())),
        ]);
        assert_eq!(
            sorted(actions(&hop.node_id, &plans, &sources, &current_routes)),
            [
                Action::Destroy { source_id: 3 },
                Action::Source {
                    hop_id: missing.id.clone(),
                    change: SourceChange::Build(wanted.source)
                },
            ]
        );
    }

    #[test]
    fn a_running_hop_is_retapped_and_rerouted_in_place() {
        let hop = sender();
        let mut was = plan(&hop, &inventory()).unwrap();
        was.source = SourceSpec::Tap {
            pids: vec![731, 732],
            mode: TapMode::Mirror,
        };
        let gone = Destination::Srt(SrtSocket {
            mode: SrtMode::Call {
                host: "192.168.1.99".into(),
                port: 23001,
            },
            latency_ms: 120,
            passphrase: None,
            key_size: None,
        });
        was.routes.push(("old".into(), gone));
        let (source, routes) = built(&hop, &was, 1, 0);

        let mut wanted = plan(&hop, &inventory()).unwrap();
        let added = Destination::Device {
            uid: "elsewhere".into(),
        };
        wanted.routes.push(("new".into(), added.clone()));
        let plans = BTreeMap::from([(hop.id.clone(), Ok(wanted))]);
        assert_eq!(
            sorted(actions(&hop.node_id, &plans, &[source], &routes)),
            [
                Action::AddRoute {
                    hop_id: hop.id.clone(),
                    source_id: 1,
                    branch_id: "new".into(),
                    destination: added
                },
                Action::Source {
                    hop_id: hop.id.clone(),
                    change: SourceChange::RemoveRoute { route_id: 11 }
                },
                Action::Source {
                    hop_id: hop.id.clone(),
                    change: SourceChange::Retap {
                        source_id: 1,
                        pids: vec![731]
                    }
                },
            ]
        );
    }

    #[test]
    fn a_route_built_twice_keeps_its_first_copy() {
        let hop = sender();
        let wanted = plan(&hop, &inventory()).unwrap();
        let (source, mut routes) = built(&hop, &wanted, 1, 0);
        let (_, copies) = built(&hop, &wanted, 1, 0);
        routes.extend(copies.into_iter().map(|route| RouteInfo {
            route_id: 99,
            ..route
        }));
        let plans = BTreeMap::from([(hop.id.clone(), Ok(wanted))]);
        assert_eq!(
            actions(&hop.node_id, &plans, &[source], &routes),
            [Action::Source {
                hop_id: hop.id.clone(),
                change: SourceChange::RemoveRoute { route_id: 99 }
            }]
        );
    }

    #[test]
    fn a_receiver_whose_socket_changed_is_rebuilt() {
        let hop = receiver();
        let mut was = plan(&hop, &inventory()).unwrap();
        was.source = SourceSpec::Srt(SrtSocket {
            mode: SrtMode::Listen { port: 23001 },
            latency_ms: 120,
            passphrase: None,
            key_size: None,
        });
        let (source, routes) = built(&hop, &was, 7, 0);
        let wanted = plan(&hop, &inventory()).unwrap();
        let plans = BTreeMap::from([(hop.id.clone(), Ok(wanted.clone()))]);
        assert_eq!(
            sorted(actions(&hop.node_id, &plans, &[source], &routes)),
            [
                Action::Destroy { source_id: 7 },
                Action::Source {
                    hop_id: hop.id.clone(),
                    change: SourceChange::Build(wanted.source)
                },
            ]
        );
    }

    #[test]
    fn a_failing_source_backs_off_until_its_plan_changes_or_it_settles() {
        let hop = sender();
        let plan = plan(&hop, &inventory()).unwrap();
        let mut retries = Retries::default();
        let start = Instant::now();
        assert!(retries.source_due(&hop.id, &plan, start));
        retries.source_failed(&hop.id, &plan, start);
        assert!(retries.source_failing(&hop.id, &plan));
        assert!(!retries.source_due(&hop.id, &plan, start + Duration::from_millis(500)));
        assert!(retries.source_due(&hop.id, &plan, start + FIRST_RETRY));
        retries.source_failed(&hop.id, &plan, start + FIRST_RETRY);
        assert!(!retries.source_due(&hop.id, &plan, start + FIRST_RETRY * 2));
        assert!(retries.source_due(&hop.id, &plan, start + FIRST_RETRY * 3));

        let mut changed = plan.clone();
        changed.routes.clear();
        assert!(retries.source_due(&hop.id, &changed, start));
        assert!(!retries.source_failing(&hop.id, &changed));

        let (source, routes) = built(&hop, &plan, 1, 0);
        let (current, _) = applied(&hop.node_id, std::slice::from_ref(&source), &routes);
        retries.settle(&hop.id, &plan, &current[hop.id.as_str()]);
        assert!(!retries.source_failing(&hop.id, &plan));
    }

    #[test]
    fn a_failing_route_backs_off_alone_until_it_is_built() {
        let mut hop = sender();
        hop.egresses.push(DesiredEgress {
            branch_id: "anna".into(),
            socket: webrtc_socket(7),
        });
        let wanted = plan(&hop, &inventory()).unwrap();
        let (srt, webrtc) = (&wanted.routes[0], &wanted.routes[1]);
        let mut retries = Retries::default();
        let start = Instant::now();
        retries.route_failed(&hop.id, &webrtc.0, &webrtc.1, start);
        assert!(retries.route_failing(&hop.id, &webrtc.0, &webrtc.1));
        assert!(!retries.route_due(&hop.id, &webrtc.0, &webrtc.1, start));
        assert!(retries.route_due(&hop.id, &srt.0, &srt.1, start));
        assert!(retries.source_due(&hop.id, &wanted, start));
        assert!(!retries.source_failing(&hop.id, &wanted));

        let regenerated = Destination::WebRtc(webrtc_peer(8, PeerRole::Offer));
        assert!(retries.route_due(&hop.id, &webrtc.0, &regenerated, start));

        let (source, routes) = built(&hop, &wanted, 1, 0);
        let (current, _) = applied(&hop.node_id, std::slice::from_ref(&source), &routes);
        retries.settle(&hop.id, &wanted, &current[hop.id.as_str()]);
        assert!(!retries.route_failing(&hop.id, &webrtc.0, &webrtc.1));
    }

    #[test]
    fn a_refused_route_reports_lost_while_the_other_branches_keep_their_condition() {
        let mut hop = sender();
        hop.egresses.push(DesiredEgress {
            branch_id: "anna".into(),
            socket: webrtc_socket(7),
        });
        let wanted = plan(&hop, &inventory()).unwrap();
        let (source, mut routes) = built(&hop, &wanted, 1, 1_000);
        routes.truncate(1);
        let (current, _) = applied(&hop.node_id, std::slice::from_ref(&source), &routes);
        let status = |retries: &Retries| {
            hop_status(
                &hop,
                &Ok(wanted.clone()),
                current.get(hop.id.as_str()),
                retries,
                &mut Rates::default(),
                &Refusals::default(),
                Instant::now(),
            )
        };

        let adding = status(&Retries::default());
        assert_eq!(adding.state, HopState::Pending);
        assert_eq!(adding.egresses[0].status.condition, LinkCondition::Flowing);
        assert_eq!(adding.egresses[1].status.condition, LinkCondition::Idle);

        let mut retries = Retries::default();
        let (branch_id, destination) = &wanted.routes[1];
        retries.route_failed(&hop.id, branch_id, destination, Instant::now());
        let refused = status(&retries);
        assert_eq!(refused.state, HopState::Provisioned);
        assert_eq!(refused.ingress.condition, LinkCondition::Flowing);
        assert_eq!(refused.egresses[0].branch_id, "filip");
        assert_eq!(refused.egresses[0].status.condition, LinkCondition::Flowing);
        assert_eq!(refused.egresses[1].branch_id, "anna");
        assert_eq!(refused.egresses[1].status.condition, LinkCondition::Lost);
        assert_eq!(
            refused.egresses[1].status.peer,
            Some(PeerReport {
                link: PeerLinkId::new("weave-music-receiver-filip"),
                generation: 7,
                relayed: false,
            })
        );
    }

    #[test]
    fn a_passphrase_srt_cannot_use_fails_the_hop() {
        let mut hop = sender();
        hop.egresses[0].socket = SocketSpec::Srt(WeaveSrt::Connect {
            host: "192.168.1.30".into(),
            port: 23000,
            params: SrtParams {
                passphrase: Some(Passphrase::new("x".repeat(80))),
                ..SrtParams::default()
            },
        });
        assert!(plan(&hop, &inventory()).is_err());
    }

    #[test]
    fn status_reports_each_branch_from_its_route_and_rates_from_byte_deltas() {
        let hop = sender();
        let wanted = plan(&hop, &inventory()).unwrap();
        let mut rates = Rates::default();
        let start = Instant::now();
        let (source, routes) = built(&hop, &wanted, 1, 1_000);
        let (current, _) = applied(&hop.node_id, std::slice::from_ref(&source), &routes);
        let first = hop_status(
            &hop,
            &Ok(wanted.clone()),
            current.get(hop.id.as_str()),
            &Retries::default(),
            &mut rates,
            &Refusals::default(),
            start,
        );
        assert_eq!(first.state, HopState::Provisioned);
        assert_eq!(first.revision, hop.revision());
        assert_eq!(first.ingress.condition, LinkCondition::Flowing);
        assert_eq!(first.egresses[0].branch_id, "filip");
        assert_eq!(
            first.egresses[0].status.resolved,
            Some(ResolvedAddr {
                host: "192.168.1.30".into(),
                port: 23000
            })
        );
        let stats = first.egresses[0].status.stats.unwrap();
        assert_eq!(
            (stats.packets_sent_lost, stats.packets_retransmitted),
            (2, 5)
        );

        let (source, routes) = built(&hop, &wanted, 1, 1_000 + 125_000);
        let (current, _) = applied(&hop.node_id, std::slice::from_ref(&source), &routes);
        let later = start + std::time::Duration::from_secs(1);
        let second = hop_status(
            &hop,
            &Ok(wanted),
            current.get(hop.id.as_str()),
            &Retries::default(),
            &mut rates,
            &Refusals::default(),
            later,
        );
        let rate = second.egresses[0].status.stats.unwrap().rate_mbps;
        assert!((rate - 1.0).abs() < 1e-9, "{rate}");
    }

    #[test]
    fn an_unbuilt_or_refused_hop_reports_every_branch_idle() {
        let hop = receiver();
        let mut rates = Rates::default();
        let pending = hop_status(
            &hop,
            &plan(&hop, &inventory()),
            None,
            &Retries::default(),
            &mut rates,
            &Refusals::default(),
            Instant::now(),
        );
        assert_eq!(pending.state, HopState::Pending);
        assert_eq!(pending.ingress.condition, LinkCondition::Idle);
        assert_eq!(
            pending.ingress.resolved,
            Some(ResolvedAddr {
                host: "0.0.0.0".into(),
                port: 23000
            })
        );
        assert_eq!(pending.egresses.len(), 1);
        let refused = hop_status(
            &hop,
            &Err("no".into()),
            None,
            &Retries::default(),
            &mut rates,
            &Refusals::default(),
            Instant::now(),
        );
        assert_eq!(refused.state, HopState::Failed);
    }
}
