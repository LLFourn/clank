//! Finding the address of a name that was minted seconds ago.
//!
//! RFC 2308 §5 caches a negative answer by name and class, NOT by
//! record type. One query of any type for a name that is not yet
//! published therefore burns that name — for this process, for every
//! other process on the machine, and for the browser the user is about
//! to open — until the zone's SOA minimum expires. For
//! `trycloudflare.com` that minimum is 1800 seconds, and a tunnel
//! hostname is published a good ten seconds after the connector
//! announces it.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

type Fut<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'a>>;

/// What an authoritative server said about a name: the addresses it
/// has, the name it points at instead, or the servers it says to ask.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Reply {
    pub(crate) addrs: Vec<IpAddr>,
    pub(crate) cname: Option<String>,
    pub(crate) referral: Vec<String>,
}

/// The two ways a question can be asked, kept apart because only one
/// of them is safe for a name that did not exist a moment ago.
///
/// The recursive methods are named for the cache they touch. Nothing
/// passes the freshly minted leaf to either of them — [`ancestors`]
/// makes that structural rather than careful.
pub(crate) trait Ask: Send + Sync {
    /// The nameservers for `zone`, through the machine's configured
    /// resolver. Only ever asked about ancestors of the leaf, which
    /// pre-date the tunnel by definition.
    fn zone_servers<'a>(&'a self, zone: &'a str) -> Fut<'a, Vec<String>>;

    /// Addresses for `name`, through the machine's configured
    /// resolver. Only ever asked about nameserver names and CNAME
    /// targets — shared infrastructure, not a per-start mint.
    fn addresses<'a>(&'a self, name: &'a str) -> Fut<'a, Vec<IpAddr>>;

    /// Addresses for `name`, asked straight of `servers`. They answer
    /// from the zone rather than from a cache, so there is no negative
    /// cache between us and the origin. This is the only path the
    /// ephemeral leaf is ever asked on.
    fn direct<'a>(&'a self, servers: &'a [SocketAddr], name: &'a str) -> Fut<'a, Reply>;
}

/// Every proper ancestor of `host`, nearest first, down to the TLD.
///
/// `host` itself is deliberately absent: this is the list of names the
/// caching resolver may be asked about, and the leaf is the one name
/// it must never hear.
pub(crate) fn ancestors(host: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = host;
    while let Some((_, parent)) = rest.split_once('.') {
        if parent.is_empty() {
            break;
        }
        out.push(parent);
        rest = parent;
    }
    out
}

/// How many delegations to follow before calling it a loop.
const HOPS: usize = 4;

/// The address of `host`, found without ever asking a caching resolver
/// about `host`.
///
/// An `IpAddr` host answers immediately, with no resolver and no
/// network. Everything else is resolved at the zone's own servers.
pub(crate) async fn address(
    ask: &dyn Ask,
    host: &str,
    port: u16,
    deadline: tokio::time::Instant,
) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    // One place owns the deadline, and it is outside every await.
    // Checking it only between attempts would let a single hung
    // lookup outlast the grace the whole start shares.
    tokio::time::timeout_at(deadline, seek(ask, host, port))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "`{host}` was not published by its own nameservers within the grace"
            ))
        })
}

async fn seek(ask: &dyn Ask, host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut servers = authority(ask, host).await?;
    let mut hops = 0;
    loop {
        match ask.direct(&servers, host).await {
            Ok(r) if !r.addrs.is_empty() => return Ok(SocketAddr::new(r.addrs[0], port)),
            Ok(r) if !r.referral.is_empty() && hops < HOPS => {
                // A delegation is an answer about where to ask next,
                // not a failure — and the next hop is still direct.
                let next = resolve_all(ask, &r.referral).await;
                if !next.is_empty() {
                    servers = next;
                    hops += 1;
                    continue;
                }
            }
            Ok(r) => {
                if let Some(target) = r.cname {
                    // The target is a different name, and shared
                    // infrastructure rather than a per-start mint, so
                    // the caching resolver may hear about it.
                    let addrs = ask.addresses(&target).await?;
                    if let Some(ip) = addrs.first() {
                        return Ok(SocketAddr::new(*ip, port));
                    }
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The authoritative servers for the closest ancestor of `host` that
/// has any — never for `host` itself.
async fn authority(ask: &dyn Ask, host: &str) -> Result<Vec<SocketAddr>, String> {
    for zone in ancestors(host) {
        let Ok(names) = ask.zone_servers(zone).await else {
            continue;
        };
        let servers = resolve_all(ask, &names).await;
        if !servers.is_empty() {
            return Ok(servers);
        }
    }
    Err(format!("no nameserver answered for any parent of `{host}`"))
}

async fn resolve_all(ask: &dyn Ask, names: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for name in names {
        for ip in ask.addresses(name).await.unwrap_or_default() {
            out.push(SocketAddr::new(ip, 53));
        }
    }
    out
}

/// [`Ask`] over the actual network.
///
/// The resolver is built on first use, so a host that is already an
/// address never constructs one at all.
#[derive(Default)]
pub(crate) struct Net {
    resolver: tokio::sync::OnceCell<hickory_resolver::TokioAsyncResolver>,
}

impl Net {
    /// There is deliberately no fallback when this fails: the
    /// recursive path is the one this module exists to keep the leaf
    /// away from, and a tunnel that does not come up costs a retry,
    /// where a burned name costs half an hour and the browser with it.
    async fn resolver(&self) -> Result<&hickory_resolver::TokioAsyncResolver, String> {
        self.resolver
            .get_or_try_init(|| async {
                hickory_resolver::TokioAsyncResolver::tokio_from_system_conf()
                    .map_err(|e| format!("no system resolver to find the zone with: {e}"))
            })
            .await
    }
}

impl Ask for Net {
    fn zone_servers<'a>(&'a self, zone: &'a str) -> Fut<'a, Vec<String>> {
        Box::pin(async move {
            let found = self
                .resolver()
                .await?
                .ns_lookup(fqdn(zone))
                .await
                .map_err(|e| e.to_string())?;
            Ok(found.iter().map(|ns| ns.0.to_utf8()).collect())
        })
    }

    fn addresses<'a>(&'a self, name: &'a str) -> Fut<'a, Vec<IpAddr>> {
        Box::pin(async move {
            let found = self
                .resolver()
                .await?
                .lookup_ip(fqdn(name))
                .await
                .map_err(|e| e.to_string())?;
            Ok(found.iter().collect())
        })
    }

    fn direct<'a>(&'a self, servers: &'a [SocketAddr], name: &'a str) -> Fut<'a, Reply> {
        Box::pin(async move {
            let mut last = "no servers to ask".to_string();
            for server in servers {
                match query(*server, name).await {
                    Ok(reply) => return Ok(reply),
                    Err(why) => last = why,
                }
            }
            Err(last)
        })
    }
}

fn fqdn(name: &str) -> String {
    // A trailing dot stops the resolver appending search domains,
    // which would turn one question into several about names that do
    // not exist.
    match name.ends_with('.') {
        true => name.to_string(),
        false => format!("{name}."),
    }
}

/// One question, to one authoritative server, over UDP.
async fn query(server: SocketAddr, name: &str) -> Result<Reply, String> {
    use hickory_client::client::{AsyncClient, ClientHandle};
    use hickory_client::proto::rr::{DNSClass, Name, RData, RecordType};
    use hickory_client::proto::udp::UdpClientStream;

    let name = Name::from_utf8(name).map_err(|e| e.to_string())?;
    let stream =
        UdpClientStream::<tokio::net::UdpSocket>::with_timeout(server, Duration::from_secs(3));
    let (mut client, background) = AsyncClient::connect(stream)
        .await
        .map_err(|e| e.to_string())?;
    let pump = tokio::spawn(background);
    let answered = client.query(name, DNSClass::IN, RecordType::A).await;
    pump.abort();
    let answered = answered.map_err(|e| e.to_string())?;

    let mut reply = Reply::default();
    for record in answered.answers() {
        match record.data() {
            Some(RData::A(a)) => reply.addrs.push(IpAddr::V4(a.0)),
            Some(RData::AAAA(a)) => reply.addrs.push(IpAddr::V6(a.0)),
            Some(RData::CNAME(target)) => reply.cname = Some(target.0.to_utf8()),
            _ => {}
        }
    }
    for record in answered.name_servers() {
        if let Some(RData::NS(ns)) = record.data() {
            reply.referral.push(ns.0.to_utf8());
        }
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    /// A zone that records WHICH path every question took. The whole
    /// point of the module is that one name never travels one of them.
    #[derive(Default)]
    struct Fake {
        /// Never answer a direct query, to stand for a nameserver
        /// that accepts the packet and says nothing.
        stall: bool,
        delegations: HashMap<String, Vec<String>>,
        known: HashMap<String, Vec<IpAddr>>,
        replies: Mutex<VecDeque<Reply>>,
        recursively: Mutex<Vec<String>>,
        directly: Mutex<Vec<String>>,
    }

    impl Fake {
        fn delegating(mut self, zone: &str, servers: &[&str]) -> Self {
            self.delegations.insert(
                zone.to_string(),
                servers.iter().map(|s| s.to_string()).collect(),
            );
            self
        }
        fn knowing(mut self, name: &str, addrs: &[&str]) -> Self {
            self.known.insert(
                name.to_string(),
                addrs.iter().map(|a| a.parse().unwrap()).collect(),
            );
            self
        }
        fn stalling(mut self) -> Self {
            self.stall = true;
            self
        }
        fn answering(self, reply: Reply) -> Self {
            self.replies.lock().unwrap().push_back(reply);
            self
        }
        fn recursive_log(&self) -> Vec<String> {
            self.recursively.lock().unwrap().clone()
        }
        fn direct_log(&self) -> Vec<String> {
            self.directly.lock().unwrap().clone()
        }
    }

    impl Ask for Fake {
        fn zone_servers<'a>(&'a self, zone: &'a str) -> Fut<'a, Vec<String>> {
            self.recursively.lock().unwrap().push(format!("NS {zone}"));
            Box::pin(async move {
                self.delegations
                    .get(zone)
                    .cloned()
                    .ok_or_else(|| format!("no NS for {zone}"))
            })
        }
        fn addresses<'a>(&'a self, name: &'a str) -> Fut<'a, Vec<IpAddr>> {
            self.recursively.lock().unwrap().push(format!("A {name}"));
            Box::pin(async move {
                self.known
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no A for {name}"))
            })
        }
        fn direct<'a>(&'a self, servers: &'a [SocketAddr], name: &'a str) -> Fut<'a, Reply> {
            self.directly
                .lock()
                .unwrap()
                .push(format!("{name} @{}", servers.len()));
            Box::pin(async move {
                if self.stall {
                    // A server that takes the question and never
                    // answers: the await that the deadline has to
                    // enclose rather than sit between.
                    std::future::pending::<()>().await;
                }
                Ok(self.replies.lock().unwrap().pop_front().unwrap_or_default())
            })
        }
    }

    const LEAF: &str = "abc.trycloudflare.com";

    fn published() -> Reply {
        Reply {
            addrs: vec!["104.16.230.132".parse().unwrap()],
            ..Default::default()
        }
    }

    fn zone() -> Fake {
        Fake::default()
            .delegating("trycloudflare.com", &["kevin.ns.cloudflare.com"])
            .knowing("kevin.ns.cloudflare.com", &["203.0.113.9"])
    }

    fn within(secs: u64) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(secs)
    }

    /// The invariant, and the only test that matters if the others
    /// pass: one query of ANY type for a name that is not yet
    /// published burns it for thirty minutes, so the leaf must reach
    /// the authoritative path and nothing else — including for `NS`.
    #[tokio::test(start_paused = true)]
    async fn the_leaf_never_travels_the_caching_path() {
        // Unpublished on the first ask, published on the second: the
        // race this module exists for.
        let fake = zone().answering(Reply::default()).answering(published());
        let got = address(&fake, LEAF, 443, within(30)).await.unwrap();
        assert_eq!(got, "104.16.230.132:443".parse().unwrap());

        let asked = fake.recursive_log();
        assert!(
            !asked.iter().any(|q| q.contains(LEAF)),
            "the leaf reached the caching resolver: {asked:?}"
        );
        assert_eq!(
            asked,
            vec!["NS trycloudflare.com", "A kevin.ns.cloudflare.com"]
        );
        assert!(
            fake.direct_log().iter().all(|q| q.starts_with(LEAF)),
            "the leaf is the only thing asked directly: {:?}",
            fake.direct_log()
        );
    }

    /// The walk starts at the parent. Listing the leaf here at all
    /// would be the bug: `ancestors` is what makes the invariant
    /// structural rather than careful.
    #[test]
    fn the_walk_starts_above_the_leaf() {
        assert_eq!(ancestors(LEAF), vec!["trycloudflare.com", "com"]);
        assert_eq!(
            ancestors("a.b.c.example.com"),
            vec!["b.c.example.com", "c.example.com", "example.com", "com"]
        );
        assert!(ancestors("localhost").is_empty());
        assert!(!ancestors(LEAF).contains(&LEAF));
    }

    /// The first ancestor that answers wins; nothing above it is
    /// asked, and the leaf is never a candidate.
    #[tokio::test(start_paused = true)]
    async fn the_walk_stops_at_the_first_zone_that_answers() {
        let fake = zone()
            .delegating("com", &["a.gtld-servers.net"])
            .knowing("a.gtld-servers.net", &["203.0.113.1"])
            .answering(published());
        address(&fake, LEAF, 443, within(10)).await.unwrap();
        assert!(
            !fake.recursive_log().contains(&"NS com".to_string()),
            "asked past the zone that answered: {:?}",
            fake.recursive_log()
        );
    }

    /// A delegation is an answer about where to ask next, and the
    /// next ask is still direct.
    #[tokio::test(start_paused = true)]
    async fn a_referral_is_followed_directly() {
        let fake = zone()
            .knowing("child.ns.example.com", &["203.0.113.55"])
            .answering(Reply {
                referral: vec!["child.ns.example.com".to_string()],
                ..Default::default()
            })
            .answering(published());
        let got = address(&fake, LEAF, 443, within(10)).await.unwrap();
        assert_eq!(got, "104.16.230.132:443".parse().unwrap());
        assert_eq!(fake.direct_log().len(), 2, "the second ask was direct too");
        assert!(
            !fake.recursive_log().iter().any(|q| q.contains(LEAF)),
            "a referral must not put the leaf on the caching path: {:?}",
            fake.recursive_log()
        );
    }

    /// An address is already an address. Tests rely on this to stay
    /// offline, and so does every `127.0.0.1` tunnel.
    #[tokio::test(start_paused = true)]
    async fn an_address_needs_no_resolver_at_all() {
        let fake = Fake::default();
        let got = address(&fake, "127.0.0.1", 8080, within(1)).await.unwrap();
        assert_eq!(got, "127.0.0.1:8080".parse().unwrap());
        assert!(fake.recursive_log().is_empty(), "asked about an address");
        assert!(fake.direct_log().is_empty(), "asked about an address");
    }

    /// A nameserver that takes the question and never answers must
    /// not outlast the grace the whole start shares. The deadline has
    /// to sit OUTSIDE the awaits, not between them.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_lookup_still_ends_at_the_deadline() {
        let fake = zone().stalling();
        let began = tokio::time::Instant::now();
        let why = address(&fake, LEAF, 443, within(5)).await.unwrap_err();
        assert!(why.contains("within the grace"), "{why}");
        assert!(
            began.elapsed() < Duration::from_secs(7),
            "a stalled lookup ran past the grace: {:?}",
            began.elapsed()
        );
        // Asked once and never answered. More than one entry would
        // mean the query returned and the retry loop is what the
        // deadline caught — a different bug from the one fixed here.
        assert_eq!(
            fake.direct_log().len(),
            1,
            "the deadline must bound a PENDING await, not a retry loop: {:?}",
            fake.direct_log()
        );
    }

    /// Giving up says DNS, and gives up WITHOUT falling back to the
    /// path that would burn the name on the way out.
    #[tokio::test(start_paused = true)]
    async fn giving_up_does_not_fall_back_to_the_cache() {
        let fake = zone();
        let why = address(&fake, LEAF, 443, within(5)).await.unwrap_err();
        assert!(
            why.contains("not published by its own nameservers"),
            "the reason must name DNS: {why}"
        );
        assert!(
            !fake.recursive_log().iter().any(|q| q.contains(LEAF)),
            "fell back to the caching resolver on the way out: {:?}",
            fake.recursive_log()
        );
        assert!(
            fake.direct_log().len() > 1,
            "it kept asking until the deadline"
        );
    }
}
