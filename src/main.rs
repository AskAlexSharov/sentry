#![allow(dead_code)]

use crate::{
    config::*,
    eth::*,
    grpc::{
        control::{InboundMessage, InboundMessageId},
        sentry::sentry_server::SentryServer,
    },
    services::*,
};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use clap::Clap;
use devp2p::*;
use educe::Educe;
use enr::CombinedKey;
use ethereum::Transaction;
use ethereum_forkid::ForkFilter;
use futures::stream::{BoxStream, StreamExt};
use k256::ecdsa::SigningKey;
use maplit::btreemap;
use num_traits::{FromPrimitive, ToPrimitive};
use parking_lot::RwLock;
use rand::thread_rng;
use rlp::Rlp;
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet},
    convert::TryFrom,
    fmt::Debug,
    sync::Arc,
    time::Duration,
};
use task_group::TaskGroup;
use tokio::sync::{
    mpsc::{channel, Sender},
    Mutex as AsyncMutex,
};
use tonic::transport::Server;
use tracing::*;
use tracing_subscriber::EnvFilter;
use trust_dns_resolver::{config::*, TokioAsyncResolver};

mod config;
mod eth;
mod grpc;
mod services;
mod types;

#[derive(Debug)]
struct DummyControl;

#[async_trait]
impl Control for DummyControl {
    async fn forward_inbound_message(&self, message: InboundMessage) -> anyhow::Result<()> {
        debug!("Received inbound message: {:?}", message);
        Ok(())
    }
    async fn get_status_data(&self) -> anyhow::Result<StatusData> {
        bail!("Not implemented")
    }
}

type OutboundSender = Sender<OutboundEvent>;
type OutboundReceiver = Arc<AsyncMutex<BoxStream<'static, OutboundEvent>>>;

#[derive(Clone)]
struct Pipes {
    sender: OutboundSender,
    receiver: OutboundReceiver,
}

#[derive(Clone, Debug, Default)]
struct BlockTracker {
    block_by_peer: HashMap<PeerId, u64>,
    peers_by_block: BTreeMap<u64, HashSet<PeerId>>,
}

impl BlockTracker {
    fn set_block_number(&mut self, peer: PeerId, block: u64) {
        if let Some(old_block) = self.block_by_peer.insert(peer, block) {
            if let Entry::Occupied(mut entry) = self.peers_by_block.entry(old_block) {
                entry.get_mut().remove(&peer);

                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
        self.peers_by_block.entry(block).or_default().insert(peer);
    }

    fn remove_peer(&mut self, peer: PeerId) {
        if let Some(block) = self.block_by_peer.remove(&peer) {
            if let Entry::Occupied(mut entry) = self.peers_by_block.entry(block) {
                entry.get_mut().remove(&peer);

                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
    }

    fn peers_with_min_block(&self, block: u64) -> HashSet<PeerId> {
        self.peers_by_block
            .range(block..)
            .map(|(_, v)| v)
            .flatten()
            .copied()
            .collect()
    }
}

#[derive(Educe)]
#[educe(Debug)]
pub struct CapabilityServerImpl<C, DP>
where
    C: Debug,
    DP: Debug,
{
    #[educe(Debug(ignore))]
    peer_pipes: Arc<RwLock<HashMap<PeerId, Pipes>>>,
    block_tracker: Arc<RwLock<BlockTracker>>,

    status_message: Arc<RwLock<Option<(StatusData, ForkFilter)>>>,
    valid_peers: Arc<RwLock<HashSet<PeerId>>>,
    control: C,
    data_provider: DP,
}

impl<C: Control, DP: DataProvider> CapabilityServerImpl<C, DP> {
    fn setup_peer(&self, peer: PeerId, p: Pipes) {
        let mut pipes = self.peer_pipes.write();
        let mut block_tracker = self.block_tracker.write();

        assert!(pipes.insert(peer, p).is_none());
        block_tracker.set_block_number(peer, 0);
    }
    fn get_pipes(&self, peer: PeerId) -> Option<Pipes> {
        self.peer_pipes.read().get(&peer).cloned()
    }
    pub fn sender(&self, peer: PeerId) -> Option<OutboundSender> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.sender.clone())
    }
    fn receiver(&self, peer: PeerId) -> Option<OutboundReceiver> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.receiver.clone())
    }
    fn teardown_peer(&self, peer: PeerId) {
        let mut pipes = self.peer_pipes.write();
        let mut block_tracker = self.block_tracker.write();
        let mut valid_peers = self.valid_peers.write();

        pipes.remove(&peer);
        block_tracker.remove_peer(peer);
        valid_peers.remove(&peer);
    }

    pub fn all_peers(&self) -> HashSet<PeerId> {
        self.peer_pipes.read().keys().copied().collect()
    }

    pub fn connected_peers(&self) -> usize {
        self.peer_pipes.read().len()
    }

    async fn handle_event(
        &self,
        peer: PeerId,
        event: InboundEvent,
    ) -> Result<Option<Message>, DisconnectReason> {
        match event {
            InboundEvent::Disconnect { .. } => {
                self.teardown_peer(peer);
            }
            InboundEvent::Message {
                message: Message { id, data },
                ..
            } => {
                let valid_peer = self.valid_peers.read().contains(&peer);
                let message_id = MessageId::from_usize(id);
                match message_id {
                    None => {
                        warn!("Unknown message");
                    }
                    Some(MessageId::Status) => {
                        let v = rlp::decode::<StatusMessage>(&data).map_err(|e| {
                            info!("Failed to decode status message: {}! Kicking peer.", e);

                            DisconnectReason::ProtocolBreach
                        })?;

                        info!("Decoded status message: {:?}", v);

                        let status_data = self.status_message.read();
                        let mut valid_peers = self.valid_peers.write();
                        if let Some((_, fork_filter)) = &*status_data {
                            fork_filter.is_compatible(v.fork_id).map_err(|reason| {
                                info!("Kicking peer with incompatible fork ID: {:?}", reason);

                                DisconnectReason::UselessPeer
                            })?;

                            valid_peers.insert(peer);
                        }
                    }
                    Some(MessageId::GetBlockHeaders) if valid_peer => {
                        let selector = rlp::decode::<GetBlockHeaders>(&*data)
                            .map_err(|_| DisconnectReason::ProtocolBreach)?;
                        info!("Block headers requested: {:?}", selector);

                        let selector = if selector.max_headers > 1 {
                            // Just one. Fast case.
                            vec![selector.block]
                        } else {
                            async {
                                let anchor = match selector.block {
                                    BlockId::Number(num) => num,
                                    BlockId::Hash(hash) => {
                                        match self.data_provider.resolve_block_height(hash).await {
                                            Ok(Some(height)) => height,
                                            Ok(None) => {
                                                // this block does not exist, exit early.
                                                return vec![];
                                            }
                                            Err(e) => {
                                                warn!("Failed to resolve block {}: {}. Will query this one hash only", hash, e);
                                                return vec![BlockId::Hash(hash)];
                                            }
                                        }
                                    }
                                };

                                if selector.skip == 0 {
                                    return vec![BlockId::Number(anchor)];
                                }

                                std::iter::once(anchor)
                                    .chain((0..selector.max_headers).map(|i| {
                                        if selector.reverse {
                                            anchor - selector.skip * i
                                        } else {
                                            anchor + selector.skip * i
                                        }
                                    }))
                                    .map(BlockId::Number)
                                    .collect()
                            }
                            .await
                        };

                        let output = self
                            .data_provider
                            .get_block_headers(selector)
                            .filter_map(|res| async move {
                                match res {
                                    Err(e) => {
                                        warn!("{}", e);
                                        None
                                    }
                                    Ok(v) => Some(v),
                                }
                            })
                            .collect::<Vec<_>>()
                            .await;

                        let id = MessageId::BlockHeaders;
                        let data = rlp::encode_list(&output);

                        info!("Replying: {:?} / {}", id, hex::encode(&data));

                        return Ok(Some(Message {
                            id: id.to_usize().unwrap(),
                            data: data.into(),
                        }));
                    }
                    Some(MessageId::GetBlockBodies) if valid_peer => {
                        let blocks = Rlp::new(&*data)
                            .as_list()
                            .map_err(|_| DisconnectReason::ProtocolBreach)?;
                        info!("Block bodies requested: {:?}", blocks);

                        let output: Vec<_> = self
                            .data_provider
                            .get_block_bodies(blocks)
                            .filter_map(|res| async move {
                                match res {
                                    Err(e) => {
                                        warn!("{}", e);
                                        None
                                    }
                                    Ok(v) => Some(v),
                                }
                            })
                            .collect::<Vec<_>>()
                            .await;

                        return Ok(Some(Message {
                            id: MessageId::BlockBodies.to_usize().unwrap(),
                            data: rlp::encode_list(&output).into(),
                        }));
                    }
                    Some(MessageId::BlockHeaders)
                    | Some(MessageId::BlockBodies)
                    | Some(MessageId::NewBlock)
                    | Some(MessageId::NewBlockHashes)
                        if valid_peer =>
                    {
                        let _ = self
                            .control
                            .forward_inbound_message(InboundMessage {
                                id: InboundMessageId::try_from(message_id.unwrap()).unwrap() as i32,
                                data: data.to_vec(),
                                peer_id: peer.as_fixed_bytes().to_vec(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }
        }

        Ok(None)
    }
}

#[async_trait]
impl<C: Control, DP: DataProvider> CapabilityServer for CapabilityServerImpl<C, DP> {
    #[instrument(skip(self, peer), level = "debug", fields(peer=&*peer.to_string()))]
    fn on_peer_connect(&self, peer: PeerId, _: BTreeSet<CapabilityId>) {
        let first_events = if let Some((status_data, fork_filter)) = &*self.status_message.read() {
            let status_message = StatusMessage {
                protocol_version: 64,
                network_id: status_data.network_id,
                total_difficulty: status_data.total_difficulty,
                best_hash: status_data.best_hash,
                genesis_hash: status_data.fork_data.genesis,
                fork_id: fork_filter.current(),
            };

            vec![
                OutboundEvent::Message {
                    capability_name: capability_name(),
                    message: Message {
                        id: MessageId::Status.to_usize().unwrap(),
                        data: rlp::encode(&status_message).into(),
                    },
                },
                OutboundEvent::Message {
                    capability_name: capability_name(),
                    message: Message {
                        id: MessageId::Transactions.to_usize().unwrap(),
                        data: rlp::encode_list(&Vec::<Transaction>::new()).into(),
                    },
                },
            ]
        } else {
            vec![OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            }]
        };

        let (sender, receiver) = channel(1);
        let receiver = Box::pin(tokio::stream::iter(first_events).chain(receiver));
        self.setup_peer(
            peer,
            Pipes {
                sender,
                receiver: Arc::new(AsyncMutex::new(receiver)),
            },
        );
    }
    #[instrument(skip(self, peer, event), level = "debug", fields(peer=&*peer.to_string(), event=&*event.to_string()))]
    async fn on_peer_event(&self, peer: PeerId, event: InboundEvent) {
        debug!("Received message");

        if let Some(ev) = self.handle_event(peer, event).await.transpose() {
            let _ = self
                .sender(peer)
                .unwrap()
                .send(match ev {
                    Ok(message) => OutboundEvent::Message {
                        capability_name: capability_name(),
                        message,
                    },
                    Err(reason) => OutboundEvent::Disconnect { reason },
                })
                .await;
        }
    }

    #[instrument(skip(self, peer), level = "debug", fields(peer=&*peer.to_string()))]
    async fn next(&self, peer: PeerId) -> OutboundEvent {
        self.receiver(peer)
            .unwrap()
            .lock()
            .await
            .next()
            .await
            .unwrap_or(OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opts = Opts::parse();

    let secret_key = if let Some(data) = opts.node_key {
        SigningKey::new(&hex::decode(data)?)?
    } else {
        SigningKey::random(thread_rng())
    };

    let listen_addr = format!("0.0.0.0:{}", opts.listen_port);

    info!("Starting Ethereum sentry");

    info!(
        "Node ID: {}",
        hex::encode(devp2p::util::pk2id(&secret_key.verify_key()).as_bytes())
    );

    let mut discovery_tasks: Vec<Arc<AsyncMutex<dyn Discovery>>> = vec![];

    if opts.dnsdisc {
        info!("Starting DNS discovery fetch from {}", opts.dnsdisc_address);
        let dns_resolver = dnsdisc::Resolver::new(Arc::new(
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default()).await?,
        ));

        discovery_tasks.push(Arc::new(AsyncMutex::new(DnsDiscovery::new(
            Arc::new(dns_resolver),
            opts.dnsdisc_address,
            None,
        ))));
    }

    if opts.discv4 {
        info!("Starting discv4 at port {}", opts.discv4_port);
        discovery_tasks.push(Arc::new(AsyncMutex::new(Discv4::new(
            discv4::Node::new(
                format!("0.0.0.0:{}", opts.discv4_port).parse().unwrap(),
                SigningKey::new(secret_key.to_bytes().as_slice()).unwrap(),
                opts.discv4_bootnodes,
                None,
                true,
                opts.listen_port,
            )
            .await
            .unwrap(),
            20,
        ))))
    }

    if opts.discv5 {
        let mut svc = discv5::Discv5::new(
            opts.discv5_enr
                .ok_or_else(|| anyhow!("discv5 ENR not specified"))?,
            CombinedKey::Secp256k1(SigningKey::new(secret_key.to_bytes().as_slice()).unwrap()),
            Default::default(),
        )
        .map_err(|e| anyhow!("{}", e))?;
        svc.start(opts.discv5_addr.parse()?)
            .map_err(|e| anyhow!("{}", e))
            .context("Failed to start discv5")?;
        info!("Starting discv5 at {}", opts.discv5_addr);
        discovery_tasks.push(Arc::new(AsyncMutex::new(Discv5::new(svc, 20))));
    }

    if !opts.reserved_peers.is_empty() {
        info!("Enabling reserved peers: {:?}", opts.reserved_peers);
        discovery_tasks.push(Arc::new(AsyncMutex::new(
            opts.reserved_peers
                .iter()
                .map(|&NodeRecord { addr, id }| (addr, id))
                .collect::<HashMap<_, _>>(),
        )))
    }

    let tasks = Arc::new(TaskGroup::new());

    let data_provider: Arc<dyn DataProvider> = if let Some(addr) = opts.web3_addr {
        if addr.scheme() != "http" && addr.scheme() != "https" {
            bail!(
                "Invalid web3 data provider URL: {}. Should start with http:// or https://.",
                addr
            );
        }
        let addr = addr.to_string();
        info!("Using web3 data provider at {}", addr);
        Arc::new(Web3DataProvider::new(addr).context("Failed to start web3 data provider")?)
    } else {
        Arc::new(DummyDataProvider)
    };
    let control: Arc<dyn Control> = if let Some(addr) = opts.control_addr {
        Arc::new(GrpcControl::connect(addr.to_string()).await?)
    } else {
        Arc::new(DummyControl)
    };
    let status_message: Arc<RwLock<Option<(StatusData, ForkFilter)>>> = Default::default();

    tasks.spawn_with_name("Status updater", {
        let status_message = status_message.clone();
        let control = control.clone();
        let data_provider = data_provider.clone();
        async move {
            loop {
                match async {
                    let status_data = match control.get_status_data().await {
                        Err(e) => {
                            debug!(
                                "Failed to get status from control, trying from data provider: {}",
                                e
                            );
                            match data_provider.get_status_data().await {
                                Err(e) => {
                                    debug!("Failed to fetch status from data provider: {}", e);
                                    return Err(e);
                                }
                                Ok(v) => v,
                            }
                        }
                        Ok(v) => v,
                    };

                    debug!("Resolving best hash");
                    let best_block = data_provider
                        .resolve_block_height(status_data.best_hash)
                        .await
                        .context("failed to resolve best hash")?
                        .ok_or_else(|| anyhow!("invalid best hash"))?;

                    let fork_filter = ForkFilter::new(
                        best_block,
                        status_data.fork_data.genesis,
                        status_data.fork_data.forks.iter().copied(),
                    );

                    Ok((status_data, fork_filter))
                }
                .await
                {
                    Ok(s) => {
                        debug!("Setting status data to {:?}", s);
                        *status_message.write() = Some(s);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to fetch status data: {}. Server will not accept new peers.",
                            e
                        );
                        *status_message.write() = None;
                    }
                }

                tokio::time::delay_for(Duration::from_secs(5)).await;
            }
        }
    });

    let capability_server = Arc::new(CapabilityServerImpl {
        peer_pipes: Default::default(),
        block_tracker: Default::default(),
        status_message,
        valid_peers: Default::default(),
        control,
        data_provider,
    });

    let swarm = Swarm::builder()
        .with_task_group(tasks.clone())
        .with_listen_options(ListenOptions {
            discovery_tasks,
            max_peers: opts.max_peers,
            addr: listen_addr.parse().unwrap(),
        })
        .with_client_version(format!("sentry/v{}", env!("CARGO_PKG_VERSION")))
        .build(
            btreemap! { CapabilityId { name: capability_name(), version: 64 } => 17 },
            capability_server.clone(),
            secret_key,
        )
        .await?;

    info!("RLPx node listening at {}", listen_addr);

    let sentry_addr = opts.sentry_addr.parse()?;

    tasks.spawn(async move {
        let svc = SentryServer::new(SentryService::new(capability_server));

        info!("Sentry gRPC server starting on {}", sentry_addr);

        Server::builder()
            .add_service(svc)
            .serve(sentry_addr)
            .await
            .unwrap();
    });

    loop {
        info!(
            "Current peers: {}/{}.",
            swarm.connected_peers(),
            opts.max_peers
        );

        tokio::time::delay_for(Duration::from_secs(5)).await;
    }
}
