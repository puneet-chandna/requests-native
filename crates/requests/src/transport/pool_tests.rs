use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use http::uri::{Authority, Scheme};

use super::pool::{
    ConnectionLease, IdentityKey, IdleConnection, LeaseTerminal, Pool, PoolGeneration, PoolKey,
    ProxyKey, TlsPoolKey,
};

fn key(
    scheme: Scheme,
    authority: &str,
    proxy: Option<&str>,
    tls: &str,
    identity: Option<&str>,
) -> PoolKey {
    PoolKey::new(
        scheme,
        authority
            .parse::<Authority>()
            .expect("valid test authority"),
        proxy.map(ProxyKey::new),
        TlsPoolKey::new(tls),
        identity.map(IdentityKey::new),
    )
}

fn direct_key() -> PoolKey {
    key(Scheme::HTTP, "example.test:80", None, "tls-default", None)
}

fn controlled_connection(
    id: usize,
    readable: impl IntoIterator<Item = u8>,
) -> (IdleConnection, Arc<AtomicUsize>) {
    let closes = Arc::new(AtomicUsize::new(0));
    (
        IdleConnection::controlled(id, readable, Arc::clone(&closes)),
        closes,
    )
}

fn fresh_lease(pool: &Pool, key: PoolKey, connection: IdleConnection) -> ConnectionLease {
    let generation = pool.generation_number(&key);
    ConnectionLease::new(key, generation, connection)
}

fn assert_key_shape(key: &PoolKey) {
    let PoolKey {
        scheme,
        authority,
        proxy,
        tls,
        identity,
    } = key;
    let _: &Scheme = scheme;
    let _: &Authority = authority;
    let _: &Option<ProxyKey> = proxy;
    let _: &TlsPoolKey = tls;
    let _: &Option<IdentityKey> = identity;
}

fn assert_generation_shape(generation: &PoolGeneration) {
    let PoolGeneration { number, idle } = generation;
    let _: &u64 = number;
    let _: &Vec<IdleConnection> = idle;
}

#[test]
fn pool_key_includes_scheme_authority_proxy_tls_and_optional_identity() {
    let base = direct_key();
    assert_key_shape(&base);

    assert_ne!(
        base,
        key(Scheme::HTTPS, "example.test:80", None, "tls-default", None,)
    );
    assert_ne!(
        base,
        key(Scheme::HTTP, "other.test:80", None, "tls-default", None,)
    );
    assert_ne!(
        base,
        key(
            Scheme::HTTP,
            "example.test:80",
            Some("http://proxy.test:8080"),
            "tls-default",
            None,
        )
    );
    assert_ne!(
        base,
        key(Scheme::HTTP, "example.test:80", None, "tls-custom", None,)
    );
    assert_ne!(
        base,
        key(
            Scheme::HTTP,
            "example.test:80",
            None,
            "tls-default",
            Some("client-certificate-a"),
        )
    );
}

#[test]
fn current_generation_clean_eof_returns_idle_up_to_capacity_per_key() {
    let mut pool = Pool::new(2);
    let first_key = direct_key();
    let second_key = key(Scheme::HTTP, "other.test:80", None, "tls-default", None);
    let (first, first_closes) = controlled_connection(1, []);
    let (second, second_closes) = controlled_connection(2, []);
    let (overflow, overflow_closes) = controlled_connection(3, []);
    let (other_first, other_first_closes) = controlled_connection(4, []);
    let (other_second, other_second_closes) = controlled_connection(5, []);

    for (key, connection) in [
        (first_key.clone(), first),
        (first_key.clone(), second),
        (first_key.clone(), overflow),
        (second_key.clone(), other_first),
        (second_key.clone(), other_second),
    ] {
        let lease = fresh_lease(&pool, key, connection);
        assert_eq!(lease.terminal(), LeaseTerminal::Active);
        let lease = lease.complete(LeaseTerminal::CleanEof);
        assert_eq!(lease.terminal(), LeaseTerminal::CleanEof);
        pool.release(lease);
    }

    assert_eq!(pool.idle_len(&first_key), 2);
    assert_eq!(pool.idle_len(&second_key), 2);
    assert_eq!(first_closes.load(Ordering::SeqCst), 0);
    assert_eq!(second_closes.load(Ordering::SeqCst), 0);
    assert_eq!(overflow_closes.load(Ordering::SeqCst), 1);
    assert_eq!(other_first_closes.load(Ordering::SeqCst), 0);
    assert_eq!(other_second_closes.load(Ordering::SeqCst), 0);

    let first_generation_number = pool.generation_number(&first_key);
    let first_generation = pool
        .generation(&first_key)
        .expect("first key has a pool generation");
    assert_generation_shape(first_generation);
    assert_eq!(first_generation.number, first_generation_number);
    assert_eq!(first_generation.idle.len(), 2);

    let current_generation = pool.generation_number(&first_key);
    let first = pool.acquire(&first_key).expect("first idle connection");
    let second = pool.acquire(&first_key).expect("second idle connection");
    assert_eq!(first.key(), &first_key);
    assert_eq!(first.generation(), current_generation);
    assert_eq!(first.terminal(), LeaseTerminal::Active);
    assert_eq!(second.key(), &first_key);
    assert_eq!(second.generation(), current_generation);
    assert_eq!(second.terminal(), LeaseTerminal::Active);
    assert_ne!(
        first.connection().controlled_id(),
        second.connection().controlled_id()
    );
    assert!(pool.acquire(&first_key).is_none());
    assert_eq!(pool.idle_len(&second_key), 2);
}

#[test]
fn lease_terminal_is_owned_exactly_once_and_dirty_never_returns_idle() {
    let mut pool = Pool::new(2);
    let key = direct_key();
    let (dirty, dirty_closes) = controlled_connection(1, []);
    let (clean, clean_closes) = controlled_connection(2, []);

    let dirty = fresh_lease(&pool, key.clone(), dirty);
    assert_eq!(dirty.terminal(), LeaseTerminal::Active);
    let dirty = dirty.complete(LeaseTerminal::Dirty);
    assert_eq!(dirty.terminal(), LeaseTerminal::Dirty);
    let dirty = dirty.complete(LeaseTerminal::CleanEof);
    assert_eq!(dirty.terminal(), LeaseTerminal::Dirty);
    pool.release(dirty);

    assert_eq!(pool.idle_len(&key), 0);
    assert_eq!(dirty_closes.load(Ordering::SeqCst), 1);

    let clean = fresh_lease(&pool, key.clone(), clean);
    let clean = clean.complete(LeaseTerminal::CleanEof);
    let clean = clean.complete(LeaseTerminal::Dirty);
    assert_eq!(clean.terminal(), LeaseTerminal::CleanEof);
    pool.release(clean);

    assert_eq!(pool.idle_len(&key), 1);
    assert_eq!(clean_closes.load(Ordering::SeqCst), 0);
}

#[test]
fn clear_keeps_old_active_readable_then_orphans_its_clean_return() {
    let mut pool = Pool::new(2);
    let idle_key = direct_key();
    let active_key = key(
        Scheme::HTTP,
        "active-only.test:80",
        None,
        "tls-default",
        None,
    );
    assert!(pool.generation(&active_key).is_none());
    let initial_generation = pool.generation_number(&active_key);
    let (idle, idle_closes) = controlled_connection(1, []);
    let (active, active_closes) = controlled_connection(2, [b'x']);

    let idle = fresh_lease(&pool, idle_key.clone(), idle).complete(LeaseTerminal::CleanEof);
    pool.release(idle);
    let mut active = fresh_lease(&pool, active_key.clone(), active);
    assert_eq!(active.generation(), initial_generation);
    assert_eq!(active.terminal(), LeaseTerminal::Active);
    assert!(pool.generation(&active_key).is_none());

    pool.clear();

    let after_first_clear = pool.generation_number(&active_key);
    assert!(after_first_clear > initial_generation);
    let generation = pool
        .generation(&idle_key)
        .expect("clear retains the idle key's empty current generation");
    assert_generation_shape(generation);
    assert_eq!(generation.number, pool.generation_number(&idle_key));
    assert!(generation.idle.is_empty());
    assert_eq!(idle_closes.load(Ordering::SeqCst), 1);
    assert_eq!(active_closes.load(Ordering::SeqCst), 0);
    assert_eq!(active.connection_mut().read_controlled_byte(), Some(b'x'));

    let active = active.complete(LeaseTerminal::CleanEof);
    pool.release(active);

    assert!(pool.generation(&active_key).is_none());
    assert_eq!(pool.idle_len(&active_key), 0);
    assert_eq!(active_closes.load(Ordering::SeqCst), 1);

    pool.clear();

    assert!(pool.generation_number(&active_key) > after_first_clear);
    assert_eq!(idle_closes.load(Ordering::SeqCst), 1);
    assert_eq!(active_closes.load(Ordering::SeqCst), 1);
}

#[test]
fn separate_pool_instances_isolate_same_key_connections() {
    let mut first_pool = Pool::new(1);
    let mut second_pool = Pool::new(1);
    let key = direct_key();
    let (first, first_closes) = controlled_connection(1, []);
    let (second, second_closes) = controlled_connection(2, []);

    let first = fresh_lease(&first_pool, key.clone(), first).complete(LeaseTerminal::CleanEof);
    first_pool.release(first);
    let second = fresh_lease(&second_pool, key.clone(), second).complete(LeaseTerminal::CleanEof);
    second_pool.release(second);

    first_pool.clear();

    assert_eq!(first_closes.load(Ordering::SeqCst), 1);
    assert_eq!(second_closes.load(Ordering::SeqCst), 0);
    assert!(first_pool.acquire(&key).is_none());
    let second = second_pool
        .acquire(&key)
        .expect("second pool retains its own connection");
    assert_eq!(second.connection().controlled_id(), 2);
    assert_eq!(second.terminal(), LeaseTerminal::Active);
}
