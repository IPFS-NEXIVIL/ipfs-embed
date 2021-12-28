use anyhow::Result;
use fnv::FnvHashMap;
use futures::{channel::mpsc, stream::Stream};
use lazy_static::lazy_static;
use libp2p::{
    core::connection::{ConnectedPoint, ConnectionId, ListenerId},
    identify::IdentifyInfo,
    multiaddr::Protocol,
    swarm::{
        protocols_handler::DummyProtocolsHandler, DialPeerCondition, NetworkBehaviour,
        NetworkBehaviourAction, PollParameters,
    },
    Multiaddr, PeerId,
};
use libp2p_blake_streams::Head;
use libp2p_quic::PublicKey;
use prometheus::{IntCounter, IntGauge, Registry};
use std::{
    borrow::Cow,
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// a new listener has been created
    NewListener(ListenerId),
    /// the given listener started listening on this address
    NewListenAddr(ListenerId, Multiaddr),
    /// the given listener stopped listening on this address
    ExpiredListenAddr(ListenerId, Multiaddr),
    /// the given listener experienced an error
    ListenerError(ListenerId, String),
    /// the given listener was closed
    ListenerClosed(ListenerId),
    /// we received an observed address for ourselves from a peer
    NewExternalAddr(Multiaddr),
    /// an address observed earlier for ourselves has been retired since it was not refreshed
    ExpiredExternalAddr(Multiaddr),
    /// an address was added for the given peer, following a successful dailling attempt
    Discovered(PeerId),
    /// a dialling attempt for the given peer has failed
    DialFailure(PeerId, Multiaddr, String),
    /// a peer could not be reached by any known address
    ///
    /// if `prune_addresses == true` then it has been removed from the address book
    Unreachable(PeerId),
    /// a new connection has been opened to the given peer
    ConnectionEstablished(PeerId, ConnectedPoint),
    /// a connection to the given peer has been closed
    ConnectionClosed(PeerId, ConnectedPoint),
    /// the given peer signaled that its address has changed
    AddressChanged(PeerId, ConnectedPoint, ConnectedPoint),
    /// we are now connected to the given peer
    Connected(PeerId),
    /// the last connection to the given peer has been closed
    Disconnected(PeerId),
    /// the given peer subscribed to the given gossipsub or broadcast topic
    Subscribed(PeerId, String),
    /// the given peer unsubscribed from the given gossipsub or broadcast topic
    Unsubscribed(PeerId, String),
    NewHead(Head),
    Bootstrapped,
    /// the peer-info for the given peer has been updated with new information
    NewInfo(PeerId),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PeerInfo {
    protocol_version: Option<String>,
    agent_version: Option<String>,
    protocols: Vec<String>,
    addresses: FnvHashMap<Multiaddr, AddressSource>,
    rtt: Option<Rtt>,
}

impl PeerInfo {
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    pub fn agent_version(&self) -> Option<&str> {
        self.agent_version.as_deref()
    }

    pub fn protocols(&self) -> impl Iterator<Item = &str> + '_ {
        self.protocols.iter().map(|s| &**s)
    }

    pub fn addresses(&self) -> impl Iterator<Item = (&Multiaddr, AddressSource)> + '_ {
        self.addresses.iter().map(|(addr, source)| (addr, *source))
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt.map(|x| x.current)
    }

    pub fn full_rtt(&self) -> Option<Rtt> {
        self.rtt
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rtt {
    current: Duration,
    decay_3: Duration,
    decay_10: Duration,
    failures: u32,
}

impl Rtt {
    pub fn new(current: Duration) -> Self {
        Self {
            current,
            decay_3: current,
            decay_10: current,
            failures: 0,
        }
    }

    pub fn register(&mut self, current: Duration) {
        self.current = current;
        self.decay_3 = self.decay_3 * 7 / 10 + current * 3 / 10;
        self.decay_10 = self.decay_10 * 9 / 10 + current / 10;
        self.failures = 0;
    }

    pub fn register_failure(&mut self) {
        self.failures += 1;
    }

    /// Get a reference to the rtt's current.
    pub fn current(&self) -> Duration {
        self.current
    }

    /// Get a reference to the rtt's decay 3.
    pub fn decay_3(&self) -> Duration {
        self.decay_3
    }

    /// Get a reference to the rtt's decay 10.
    pub fn decay_10(&self) -> Duration {
        self.decay_10
    }

    /// Get a reference to the rtt's failures.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSource {
    Mdns,
    Kad,
    Peer,
    User,
}

lazy_static! {
    pub static ref LISTENERS: IntGauge =
        IntGauge::new("peers_listeners", "Number of listeners.").unwrap();
    pub static ref LISTEN_ADDRS: IntGauge =
        IntGauge::new("peers_listen_addrs", "Number of listen addrs.",).unwrap();
    pub static ref EXTERNAL_ADDRS: IntGauge =
        IntGauge::new("peers_external_addrs", "Number of external addresses.",).unwrap();
    pub static ref DISCOVERED: IntGauge =
        IntGauge::new("peers_discovered", "Number of discovered peers.").unwrap();
    pub static ref CONNECTED: IntGauge =
        IntGauge::new("peers_connected", "Number of connected peers.").unwrap();
    pub static ref CONNECTIONS: IntGauge =
        IntGauge::new("peers_connections", "Number of connections.").unwrap();
    pub static ref LISTENER_ERROR: IntCounter = IntCounter::new(
        "peers_listener_error",
        "Number of non fatal listener errors."
    )
    .unwrap();
    pub static ref ADDRESS_REACH_FAILURE: IntCounter = IntCounter::new(
        "peers_address_reach_failure",
        "Number of address reach failures."
    )
    .unwrap();
    pub static ref DIAL_FAILURE: IntCounter =
        IntCounter::new("peers_dial_failure", "Number of dial failures.").unwrap();
}

#[inline]
pub(crate) fn normalize_addr(addr: &mut Multiaddr, peer: &PeerId) {
    if let Some(Protocol::P2p(_)) = addr.iter().last() {
    } else {
        addr.push(Protocol::P2p((*peer).into()));
    }
}

#[inline]
fn normalize_addr_ref<'a>(addr: &'a Multiaddr, peer: &PeerId) -> Cow<'a, Multiaddr> {
    if let Some(Protocol::P2p(_)) = addr.iter().last() {
        Cow::Borrowed(addr)
    } else {
        let mut addr = addr.clone();
        addr.push(Protocol::P2p((*peer).into()));
        Cow::Owned(addr)
    }
}

trait MultiaddrExt {
    fn is_loopback(&self) -> bool;
}

impl MultiaddrExt for Multiaddr {
    fn is_loopback(&self) -> bool {
        if let Some(Protocol::Ip4(addr)) = self.iter().next() {
            if !addr.is_loopback() {
                return false;
            }
        }
        true
    }
}

#[derive(Debug)]
pub struct AddressBook {
    enable_loopback: bool,
    prune_addresses: bool,
    local_node_name: String,
    local_peer_id: PeerId,
    local_public_key: PublicKey,
    peers: FnvHashMap<PeerId, PeerInfo>,
    connections: FnvHashMap<PeerId, Multiaddr>,
    event_stream: Vec<mpsc::UnboundedSender<Event>>,
    actions: VecDeque<NetworkBehaviourAction<void::Void, void::Void>>,
}

impl AddressBook {
    pub fn new(
        local_peer_id: PeerId,
        local_node_name: String,
        local_public_key: PublicKey,
        enable_loopback: bool,
        prune_addresses: bool,
    ) -> Self {
        Self {
            enable_loopback,
            prune_addresses,
            local_node_name,
            local_peer_id,
            local_public_key,
            peers: Default::default(),
            connections: Default::default(),
            event_stream: Default::default(),
            actions: Default::default(),
        }
    }

    pub fn local_public_key(&self) -> &PublicKey {
        &self.local_public_key
    }

    pub fn local_node_name(&self) -> &str {
        &self.local_node_name
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    pub fn dial(&mut self, peer: &PeerId) {
        if peer == self.local_peer_id() {
            tracing::error!("attempting to dial self");
            return;
        }
        tracing::trace!("dialing {}", peer);
        self.actions.push_back(NetworkBehaviourAction::DialPeer {
            peer_id: *peer,
            condition: DialPeerCondition::Disconnected,
        });
    }

    pub fn add_address(&mut self, peer: &PeerId, mut address: Multiaddr, source: AddressSource) {
        if peer == self.local_peer_id() {
            return;
        }
        if !self.enable_loopback && address.is_loopback() {
            return;
        }
        let discovered = !self.peers.contains_key(peer);
        let info = self.peers.entry(*peer).or_default();
        normalize_addr(&mut address, peer);
        #[allow(clippy::map_entry)]
        if !info.addresses.contains_key(&address) {
            tracing::trace!("adding address {} from {:?}", address, source);
            info.addresses.insert(address, source);
        }
        if discovered {
            self.notify(Event::Discovered(*peer));
        }
    }

    pub fn remove_address(&mut self, peer: &PeerId, address: &Multiaddr) {
        let address = normalize_addr_ref(address, peer);
        if let Some(info) = self.peers.get_mut(peer) {
            tracing::trace!("removing address {}", address);
            info.addresses.remove(&address);
        }
    }

    pub fn peers(&self) -> impl Iterator<Item = &PeerId> + '_ {
        self.peers.keys()
    }

    pub fn connections(&self) -> impl Iterator<Item = (&PeerId, &Multiaddr)> + '_ {
        self.connections.iter().map(|(peer, addr)| (peer, addr))
    }

    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.connections.contains_key(peer) || peer == self.local_peer_id()
    }

    pub fn info(&self, peer_id: &PeerId) -> Option<&PeerInfo> {
        self.peers.get(peer_id)
    }

    pub fn set_rtt(&mut self, peer_id: &PeerId, rtt: Option<Duration>) {
        if let Some(info) = self.peers.get_mut(peer_id) {
            if let Some(duration) = rtt {
                if let Some(ref mut rtt) = info.rtt {
                    rtt.register(duration);
                } else {
                    info.rtt = Some(Rtt::new(duration));
                }
            } else if let Some(ref mut rtt) = info.rtt {
                rtt.register_failure();
            }
            self.notify(Event::NewInfo(*peer_id))
        }
    }

    pub fn set_info(&mut self, peer_id: &PeerId, identify: IdentifyInfo) {
        if let Some(info) = self.peers.get_mut(peer_id) {
            info.protocol_version = Some(identify.protocol_version);
            info.agent_version = Some(identify.agent_version);
            info.protocols = identify.protocols;
        }
    }

    pub fn swarm_events(&mut self) -> SwarmEvents {
        let (tx, rx) = mpsc::unbounded();
        self.event_stream.push(tx);
        SwarmEvents(rx)
    }

    pub fn notify(&mut self, event: Event) {
        tracing::trace!("{:?}", event);
        self.event_stream
            .retain(|tx| tx.unbounded_send(event.clone()).is_ok());
    }

    pub fn register_metrics(&self, registry: &Registry) -> Result<()> {
        registry.register(Box::new(LISTENERS.clone()))?;
        registry.register(Box::new(LISTEN_ADDRS.clone()))?;
        registry.register(Box::new(EXTERNAL_ADDRS.clone()))?;
        registry.register(Box::new(DISCOVERED.clone()))?;
        registry.register(Box::new(CONNECTED.clone()))?;
        registry.register(Box::new(CONNECTIONS.clone()))?;
        registry.register(Box::new(LISTENER_ERROR.clone()))?;
        registry.register(Box::new(ADDRESS_REACH_FAILURE.clone()))?;
        registry.register(Box::new(DIAL_FAILURE.clone()))?;
        Ok(())
    }
}

pub struct SwarmEvents(mpsc::UnboundedReceiver<Event>);

impl Stream for SwarmEvents {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

impl NetworkBehaviour for AddressBook {
    type ProtocolsHandler = DummyProtocolsHandler;
    type OutEvent = void::Void;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        Default::default()
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        if let Some(info) = self.peers.get(peer_id) {
            info.addresses().map(|(addr, _)| addr.clone()).collect()
        } else {
            vec![]
        }
    }

    fn inject_event(&mut self, _peer_id: PeerId, _connection: ConnectionId, _event: void::Void) {}

    fn poll(
        &mut self,
        _cx: &mut Context,
        _params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<void::Void, void::Void>> {
        if let Some(action) = self.actions.pop_front() {
            Poll::Ready(action)
        } else {
            Poll::Pending
        }
    }

    fn inject_connected(&mut self, peer_id: &PeerId) {
        tracing::trace!("connected to {}", peer_id);
        CONNECTED.inc();
        self.notify(Event::Connected(*peer_id));
    }

    fn inject_disconnected(&mut self, peer_id: &PeerId) {
        tracing::trace!("disconnected from {}", peer_id);
        CONNECTED.dec();
        self.notify(Event::Disconnected(*peer_id));
    }

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        _: &ConnectionId,
        conn: &ConnectedPoint,
    ) {
        let mut address = conn.get_remote_address().clone();
        normalize_addr(&mut address, peer_id);
        tracing::debug!(
            addr = display(&address),
            out = conn.is_dialer(),
            "connection established"
        );
        self.add_address(peer_id, address.clone(), AddressSource::Peer);
        self.connections.insert(*peer_id, address);
        self.notify(Event::ConnectionEstablished(*peer_id, conn.clone()));
    }

    fn inject_address_change(
        &mut self,
        peer_id: &PeerId,
        _: &ConnectionId,
        old: &ConnectedPoint,
        new: &ConnectedPoint,
    ) {
        let mut new_addr = new.get_remote_address().clone();
        normalize_addr(&mut new_addr, peer_id);
        tracing::debug!(
            old = display(old.get_remote_address()),
            new = display(&new_addr),
            out = new.is_dialer(),
            "address changed"
        );
        self.add_address(peer_id, new_addr.clone(), AddressSource::Peer);
        self.connections.insert(*peer_id, new_addr);
        self.notify(Event::AddressChanged(*peer_id, old.clone(), new.clone()));
    }

    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        _: &ConnectionId,
        conn: &ConnectedPoint,
    ) {
        let mut addr = conn.get_remote_address().clone();
        normalize_addr(&mut addr, peer_id);
        tracing::debug!(
            addr = display(&addr),
            out = conn.is_dialer(),
            "connection closed"
        );
        self.connections.remove(peer_id);
        self.notify(Event::ConnectionClosed(*peer_id, conn.clone()));
    }

    fn inject_addr_reach_failure(
        &mut self,
        peer_id: Option<&PeerId>,
        addr: &Multiaddr,
        error: &dyn std::error::Error,
    ) {
        if let Some(peer_id) = peer_id {
            let still_connected = self.is_connected(peer_id);
            let mut naddr = addr.clone();
            normalize_addr(&mut naddr, peer_id);
            let error = format!("{:#}", error);
            tracing::debug!(
                addr = display(&naddr),
                error = display(&error),
                still_connected = still_connected,
                "dial failure"
            );
            self.notify(Event::DialFailure(*peer_id, addr.clone(), error));
            if self.is_connected(peer_id) {
                return;
            }
            ADDRESS_REACH_FAILURE.inc();
            if self.prune_addresses {
                self.remove_address(peer_id, addr);
            }
        } else {
            tracing::debug!(addr = display(addr), error = display(error), "dial failure");
        }
    }

    fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        if self.prune_addresses {
            // If an address was added after the peer was dialed retry dialing the
            // peer.
            if let Some(peer) = self.peers.get(peer_id) {
                if !peer.addresses.is_empty() {
                    tracing::debug!(peer = display(peer_id), "redialing with new addresses");
                    self.dial(peer_id);
                    return;
                }
            }
        }
        tracing::trace!("dial failure {}", peer_id);
        DIAL_FAILURE.inc();
        if self.peers.contains_key(peer_id) {
            DISCOVERED.dec();
            self.notify(Event::Unreachable(*peer_id));
            if self.prune_addresses {
                self.peers.remove(peer_id);
            }
        }
    }

    fn inject_new_listener(&mut self, id: ListenerId) {
        tracing::trace!("listener {:?}: created", id);
        LISTENERS.inc();
        self.notify(Event::NewListener(id));
    }

    fn inject_new_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
        tracing::trace!("listener {:?}: new listen addr {}", id, addr);
        LISTEN_ADDRS.inc();
        self.notify(Event::NewListenAddr(id, addr.clone()));
    }

    fn inject_expired_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
        tracing::trace!("listener {:?}: expired listen addr {}", id, addr);
        LISTEN_ADDRS.dec();
        self.notify(Event::ExpiredListenAddr(id, addr.clone()));
    }

    fn inject_listener_error(&mut self, id: ListenerId, err: &(dyn std::error::Error + 'static)) {
        let err = format!("{:#}", err);
        tracing::trace!("listener {:?}: listener error {}", id, err);
        LISTENER_ERROR.inc();
        self.notify(Event::ListenerError(id, err));
    }

    fn inject_listener_closed(&mut self, id: ListenerId, reason: Result<(), &std::io::Error>) {
        tracing::trace!("listener {:?}: closed for reason {:?}", id, reason);
        LISTENERS.dec();
        self.notify(Event::ListenerClosed(id));
    }

    fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
        let mut addr = addr.clone();
        normalize_addr(&mut addr, self.local_peer_id());
        tracing::trace!("new external addr {}", addr);
        EXTERNAL_ADDRS.inc();
        self.notify(Event::NewExternalAddr(addr));
    }

    fn inject_expired_external_addr(&mut self, addr: &Multiaddr) {
        let mut addr = addr.clone();
        normalize_addr(&mut addr, self.local_peer_id());
        tracing::trace!("expired external addr {}", addr);
        EXTERNAL_ADDRS.dec();
        self.notify(Event::ExpiredExternalAddr(addr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate_keypair;
    use futures::stream::StreamExt;

    #[async_std::test]
    async fn test_dial_basic() {
        let mut book = AddressBook::new(
            PeerId::random(),
            "".into(),
            generate_keypair().public,
            false,
            true,
        );
        let mut stream = book.swarm_events();
        let peer_a = PeerId::random();
        let addr_1: Multiaddr = "/ip4/1.1.1.1/tcp/3333".parse().unwrap();
        let mut addr_1_2 = addr_1.clone();
        addr_1_2.push(Protocol::P2p(peer_a.into()));
        let addr_2: Multiaddr = "/ip4/2.2.2.2/tcp/3333".parse().unwrap();
        let error = std::io::Error::new(std::io::ErrorKind::Other, "my error");
        book.add_address(&peer_a, addr_1.clone(), AddressSource::Mdns);
        book.add_address(&peer_a, addr_1_2, AddressSource::User);
        book.add_address(&peer_a, addr_2.clone(), AddressSource::Peer);
        assert_eq!(stream.next().await, Some(Event::Discovered(peer_a)));
        let peers = book.peers().collect::<Vec<_>>();
        assert_eq!(peers, vec![&peer_a]);
        book.inject_addr_reach_failure(Some(&peer_a), &addr_1, &error);
        book.inject_addr_reach_failure(Some(&peer_a), &addr_2, &error);
        book.inject_dial_failure(&peer_a);
        assert_eq!(
            stream.next().await,
            Some(Event::DialFailure(
                peer_a,
                addr_1.clone(),
                "my error".to_owned()
            ))
        );
        assert_eq!(
            stream.next().await,
            Some(Event::DialFailure(
                peer_a,
                addr_2.clone(),
                "my error".to_owned()
            ))
        );
        assert_eq!(stream.next().await, Some(Event::Unreachable(peer_a)));
        #[allow(clippy::needless_collect)]
        let peers = book.peers().collect::<Vec<_>>();
        assert!(peers.is_empty());
    }

    #[async_std::test]
    async fn test_dial_with_added_addrs() {
        let mut book = AddressBook::new(
            PeerId::random(),
            "".into(),
            generate_keypair().public,
            false,
            true,
        );
        let mut stream = book.swarm_events();
        let peer_a = PeerId::random();
        let addr_1: Multiaddr = "/ip4/1.1.1.1/tcp/3333".parse().unwrap();
        let addr_2: Multiaddr = "/ip4/2.2.2.2/tcp/3333".parse().unwrap();
        let error = std::io::Error::new(std::io::ErrorKind::Other, "my error");
        book.add_address(&peer_a, addr_1.clone(), AddressSource::Mdns);
        assert_eq!(stream.next().await, Some(Event::Discovered(peer_a)));
        book.add_address(&peer_a, addr_2.clone(), AddressSource::Peer);
        book.inject_addr_reach_failure(Some(&peer_a), &addr_1, &error);
        book.inject_dial_failure(&peer_a);
        // book.poll
        let peers = book.peers().collect::<Vec<_>>();
        assert_eq!(peers, vec![&peer_a]);
        book.inject_addr_reach_failure(Some(&peer_a), &addr_2, &error);
        book.inject_dial_failure(&peer_a);
        assert_eq!(
            stream.next().await,
            Some(Event::DialFailure(
                peer_a,
                addr_1.clone(),
                "my error".to_owned()
            ))
        );
        assert_eq!(
            stream.next().await,
            Some(Event::DialFailure(
                peer_a,
                addr_2.clone(),
                "my error".to_owned()
            ))
        );
        assert_eq!(stream.next().await, Some(Event::Unreachable(peer_a)));
        #[allow(clippy::needless_collect)]
        let peers = book.peers().collect::<Vec<_>>();
        assert!(peers.is_empty());
    }
}
