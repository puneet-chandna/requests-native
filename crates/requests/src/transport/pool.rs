use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::fmt;
#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use http::uri::{Authority, Scheme};
use hyper::client::conn::http1::SendRequest;

use super::{ConnectionDriver, OutgoingBody};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ProxyKey(String);

impl ProxyKey {
    #[cfg(test)]
    pub(super) fn new(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TlsPoolKey(String);

impl TlsPoolKey {
    pub(super) fn plain() -> Self {
        Self("plain".to_owned())
    }

    #[cfg(test)]
    pub(super) fn platform() -> Self {
        Self("platform".to_owned())
    }

    #[cfg(test)]
    pub(super) fn pem_bundle(path: &std::path::Path) -> Self {
        Self(format!("pem-bundle:{}", path.display()))
    }

    #[cfg(test)]
    pub(super) fn pem_directory(path: &std::path::Path) -> Self {
        Self(format!("pem-directory:{}", path.display()))
    }

    #[cfg(test)]
    pub(super) fn disabled() -> Self {
        Self("disabled".to_owned())
    }

    #[cfg(test)]
    pub(super) fn new(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct IdentityKey(String);

impl IdentityKey {
    #[cfg(test)]
    pub(super) fn from_paths(
        certificate_chain: &std::path::Path,
        private_key: Option<&std::path::Path>,
    ) -> Self {
        Self(format!(
            "certificate-chain:{};private-key:{}",
            certificate_chain.display(),
            private_key.map_or_else(
                || "<combined>".to_owned(),
                |path| path.display().to_string(),
            ),
        ))
    }

    #[cfg(test)]
    pub(super) fn new(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PoolKey {
    pub(super) scheme: Scheme,
    pub(super) authority: Authority,
    pub(super) proxy: Option<ProxyKey>,
    pub(super) tls: TlsPoolKey,
    pub(super) identity: Option<IdentityKey>,
}

impl PoolKey {
    pub(super) fn new(
        scheme: Scheme,
        authority: Authority,
        proxy: Option<ProxyKey>,
        tls: TlsPoolKey,
        identity: Option<IdentityKey>,
    ) -> Self {
        Self {
            scheme,
            authority,
            proxy,
            tls,
            identity,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LeaseTerminal {
    Active,
    CleanEof,
    Dirty,
}

pub(super) struct IdleConnection {
    inner: IdleConnectionInner,
}

enum IdleConnectionInner {
    Network {
        sender: SendRequest<OutgoingBody>,
        driver: ConnectionDriver,
    },
    #[cfg(test)]
    Controlled(ControlledConnection),
}

#[cfg(test)]
struct ControlledConnection {
    id: usize,
    readable: VecDeque<u8>,
    closes: Arc<AtomicUsize>,
}

#[cfg(test)]
impl Drop for ControlledConnection {
    fn drop(&mut self) {
        self.closes.fetch_add(1, Ordering::SeqCst);
    }
}

impl fmt::Debug for IdleConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            IdleConnectionInner::Network { .. } => formatter.write_str("IdleConnection::Network"),
            #[cfg(test)]
            IdleConnectionInner::Controlled(connection) => formatter
                .debug_tuple("IdleConnection::Controlled")
                .field(&connection.id)
                .finish(),
        }
    }
}

impl IdleConnection {
    pub(super) fn network(sender: SendRequest<OutgoingBody>, driver: ConnectionDriver) -> Self {
        Self {
            inner: IdleConnectionInner::Network { sender, driver },
        }
    }

    pub(super) fn is_live(&self) -> bool {
        match &self.inner {
            IdleConnectionInner::Network { sender, driver } => {
                !sender.is_closed() && driver.is_reusable()
            }
            #[cfg(test)]
            IdleConnectionInner::Controlled(_) => true,
        }
    }

    pub(super) fn network_parts_mut(
        &mut self,
    ) -> (&mut SendRequest<OutgoingBody>, &mut ConnectionDriver) {
        match &mut self.inner {
            IdleConnectionInner::Network { sender, driver } => (sender, driver),
            #[cfg(test)]
            IdleConnectionInner::Controlled(_) => {
                panic!("controlled connection has no network parts")
            }
        }
    }

    pub(super) fn peer_is_open(&self) -> bool {
        match &self.inner {
            IdleConnectionInner::Network { driver, .. } => driver.peer_is_open(),
            #[cfg(test)]
            IdleConnectionInner::Controlled(_) => true,
        }
    }

    #[cfg(test)]
    pub(super) fn controlled(
        id: usize,
        readable: impl IntoIterator<Item = u8>,
        closes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            inner: IdleConnectionInner::Controlled(ControlledConnection {
                id,
                readable: readable.into_iter().collect(),
                closes,
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn controlled_id(&self) -> usize {
        match &self.inner {
            IdleConnectionInner::Controlled(connection) => connection.id,
            IdleConnectionInner::Network { .. } => panic!("network connection has no test id"),
        }
    }

    #[cfg(test)]
    pub(super) fn read_controlled_byte(&mut self) -> Option<u8> {
        match &mut self.inner {
            IdleConnectionInner::Controlled(connection) => connection.readable.pop_front(),
            IdleConnectionInner::Network { .. } => {
                panic!("network connection has no controlled bytes")
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct PoolGeneration {
    pub(super) number: u64,
    pub(super) idle: Vec<IdleConnection>,
}

#[derive(Debug)]
pub(super) struct ConnectionLease {
    key: PoolKey,
    generation: u64,
    terminal: LeaseTerminal,
    connection: Option<IdleConnection>,
}

impl ConnectionLease {
    pub(super) fn new(key: PoolKey, generation: u64, connection: IdleConnection) -> Self {
        Self {
            key,
            generation,
            terminal: LeaseTerminal::Active,
            connection: Some(connection),
        }
    }

    #[cfg(test)]
    pub(super) fn key(&self) -> &PoolKey {
        &self.key
    }

    #[cfg(test)]
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(super) fn terminal(&self) -> LeaseTerminal {
        self.terminal
    }

    #[cfg(test)]
    pub(super) fn connection(&self) -> &IdleConnection {
        self.connection
            .as_ref()
            .expect("active lease owns one connection")
    }

    pub(super) fn connection_mut(&mut self) -> &mut IdleConnection {
        self.connection
            .as_mut()
            .expect("active lease owns one connection")
    }

    pub(super) fn is_live(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(IdleConnection::is_live)
    }

    pub(super) fn peer_is_open(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(IdleConnection::peer_is_open)
    }

    pub(super) fn complete(mut self, terminal: LeaseTerminal) -> Self {
        if self.terminal == LeaseTerminal::Active && terminal != LeaseTerminal::Active {
            self.terminal = terminal;
        }
        self
    }
}

#[derive(Debug)]
pub(super) struct Pool {
    max_idle_per_key: usize,
    generation_number: u64,
    generations: HashMap<PoolKey, PoolGeneration>,
}

impl Pool {
    pub(super) fn new(max_idle_per_key: usize) -> Self {
        Self {
            max_idle_per_key,
            generation_number: 0,
            generations: HashMap::new(),
        }
    }

    pub(super) fn generation_number(&self, _key: &PoolKey) -> u64 {
        self.generation_number
    }

    #[cfg(test)]
    pub(super) fn generation(&self, key: &PoolKey) -> Option<&PoolGeneration> {
        self.generations.get(key)
    }

    #[cfg(test)]
    pub(super) fn idle_len(&self, key: &PoolKey) -> usize {
        self.generations
            .get(key)
            .map_or(0, |generation| generation.idle.len())
    }

    #[cfg(test)]
    pub(super) fn generation_count(&self) -> usize {
        self.generations.len()
    }

    pub(super) fn acquire(&mut self, key: &PoolKey) -> Option<ConnectionLease> {
        let generation = self.generations.get_mut(key)?;
        let connection = generation.idle.pop()?;
        Some(ConnectionLease::new(
            key.clone(),
            self.generation_number,
            connection,
        ))
    }

    pub(super) fn release(&mut self, mut lease: ConnectionLease) -> Option<ConnectionLease> {
        let reusable = lease.terminal == LeaseTerminal::CleanEof
            && lease.generation == self.generation_number
            && lease.is_live();
        if !reusable {
            return Some(lease);
        }

        let generation = self
            .generations
            .entry(lease.key.clone())
            .or_insert_with(|| PoolGeneration {
                number: self.generation_number,
                idle: Vec::new(),
            });
        if generation.number != self.generation_number
            || generation.idle.len() >= self.max_idle_per_key
        {
            return Some(lease);
        }
        generation
            .idle
            .push(lease.connection.take().expect("reusable lease connection"));
        None
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn clear(&mut self) -> Vec<IdleConnection> {
        self.generation_number = self
            .generation_number
            .checked_add(1)
            .expect("pool generation overflow");
        let mut evicted = Vec::new();
        for generation in self.generations.values_mut() {
            generation.number = self.generation_number;
            evicted.append(&mut generation.idle);
        }
        evicted
    }
}
