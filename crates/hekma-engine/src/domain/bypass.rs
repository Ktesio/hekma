//! Bypass detection (story 22-4, PRD FR-54, spine AD-23): the engine
//! composes and resolves the provider-host list; each Shim compares its
//! process tree's established TCP connections against it and reports the
//! hosts it saw; the engine applies the verdict.
//!
//! The list is global: the built-in provider hosts plus every configured
//! origin in this state directory (open Runs' upstreams, bindings'
//! upstreams, custody origins), so an agent calling a provider that is not
//! its own is caught too. Detection is a signal, never an action: it never
//! stops, pauses or fences a Run.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::runs::snapshot::RunSnapshot;

/// How often a Shim checks its tree.
pub const BYPASS_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a resolution is reused before it is refreshed.
pub const BYPASS_DNS_TTL: Duration = Duration::from_secs(60);

/// The most hosts the list carries.
pub const MAX_PROVIDER_HOSTS: usize = 512;

/// The most distinct hosts stored per Run.
pub const MAX_BYPASS_HOSTS_PER_RUN: usize = 8;

/// One listed endpoint (a host and port) and the addresses it resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHost {
    pub host: String,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
}

/// One listed endpoint before resolution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
}

/// The host and port of a URL or origin (`https://api.x.com:443/v1` →
/// `api.x.com:443`); the port defaults to the scheme's (443 for `https`
/// and a bare host, 80 for `http`).
pub(crate) fn endpoint_of(url: &str) -> Option<Endpoint> {
    let default = if url.starts_with("http://") { 80 } else { 443 };
    let host = host_of(url)?;
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let port = if let Some(after) = authority.rsplit_once("]:") {
        after.1.parse().ok()
    } else if authority.starts_with('[') {
        None
    } else {
        authority
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
    };
    Some(Endpoint {
        host,
        port: port.unwrap_or(default),
    })
}

/// The host of a URL or origin (`https://api.x.com:443/v1` → `api.x.com`).
pub(crate) fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The provider-host list for `snapshot`: built-in hosts, every open Run's
/// upstream and custody origin, every binding's upstream; deduplicated,
/// sorted, at most [`MAX_PROVIDER_HOSTS`].
pub(crate) fn compose_provider_hosts(snapshot: &RunSnapshot) -> Vec<Endpoint> {
    let mut hosts: BTreeSet<Endpoint> = super::custody::builtin_provider_hosts()
        .map(|host| Endpoint {
            host: host.to_string(),
            port: 443,
        })
        .collect();
    for entry in snapshot.runs.values() {
        if let Some(upstream) = &entry.upstream {
            hosts.extend(endpoint_of(upstream));
        }
        if let Some(custody) = &entry.custody {
            hosts.extend(endpoint_of(&custody.origin));
        }
    }
    for upstream in snapshot.bindings.upstreams() {
        hosts.extend(endpoint_of(upstream));
    }
    hosts.into_iter().take(MAX_PROVIDER_HOSTS).collect()
}

/// A resolution cache shared by every launch: a host's addresses are
/// reused for [`BYPASS_DNS_TTL`]; a failed resolution keeps the last good
/// set.
#[derive(Debug, Default)]
pub(crate) struct HostResolver {
    cache: Mutex<BTreeMap<String, (Instant, Vec<IpAddr>)>>,
}

impl HostResolver {
    /// `hosts` with their addresses (resolving the stale ones with
    /// `getaddrinfo`; an IP literal resolves to itself).
    pub(crate) fn resolve(&self, hosts: &[Endpoint]) -> Vec<ProviderHost> {
        self.resolve_with(hosts, &|host| {
            (host, 443)
                .to_socket_addrs()
                .map(|addrs| addrs.map(|addr| addr.ip()).collect())
                .map_err(|err| err.to_string())
        })
    }

    pub(crate) fn resolve_with(
        &self,
        endpoints: &[Endpoint],
        lookup: &dyn Fn(&str) -> Result<Vec<IpAddr>, String>,
    ) -> Vec<ProviderHost> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        endpoints
            .iter()
            .map(|endpoint| {
                let host = &endpoint.host;
                let fresh = cache
                    .get(host)
                    .is_some_and(|(at, _)| at.elapsed() < BYPASS_DNS_TTL);
                if !fresh {
                    let resolved = match host.parse::<IpAddr>() {
                        Ok(ip) => Ok(vec![ip]),
                        Err(_) => lookup(host),
                    };
                    match resolved {
                        Ok(mut addrs) => {
                            addrs.sort();
                            addrs.dedup();
                            cache.insert(host.clone(), (Instant::now(), addrs));
                        }
                        // Keep the last good set (or none yet).
                        Err(_) => {
                            cache
                                .entry(host.clone())
                                .or_insert_with(|| (Instant::now(), Vec::new()));
                        }
                    }
                }
                ProviderHost {
                    host: host.clone(),
                    port: endpoint.port,
                    addrs: cache.get(host).map(|(_, a)| a.clone()).unwrap_or_default(),
                }
            })
            .collect()
    }
}

/// The resolved provider-host list every Shim compares against, shared by
/// the launch path (the list a new Shim starts with) and the refresh (which
/// re-composes and re-resolves it at most every [`BYPASS_DNS_TTL`] and
/// pushes a changed list to every live Shim).
#[derive(Debug, Default)]
pub(crate) struct BypassFeed {
    resolver: HostResolver,
    list: Mutex<Vec<ProviderHost>>,
    /// When the list was last composed, and from which published snapshot
    /// (its address: a changed snapshot may carry a new origin).
    refreshed: Mutex<Option<(Instant, usize)>>,
}

impl BypassFeed {
    /// The configuration a Shim is launched with (or pushed); `None` until
    /// a first refresh has composed a list — a Shim given no list reports
    /// nothing rather than reading as checked.
    pub(crate) fn config(&self) -> Option<crate::shim::proto::BypassConfig> {
        let list = self.list.lock().unwrap_or_else(|p| p.into_inner());
        (!list.is_empty()).then(|| crate::shim::proto::BypassConfig {
            interval_ms: BYPASS_POLL_INTERVAL.as_millis() as u64,
            hosts: list.clone(),
        })
    }

    /// The listed host names (what a Shim may report).
    pub(crate) fn listed(&self) -> BTreeSet<String> {
        let list = self.list.lock().unwrap_or_else(|p| p.into_inner());
        list.iter().map(|entry| entry.host.clone()).collect()
    }

    /// Whether the list is due a refresh: never composed, older than the
    /// TTL, or the published snapshot changed (a new binding, upstream or
    /// custody origin) at least one poll interval after the last one — a
    /// pointer compare per call, never a walk.
    pub(crate) fn refresh_due(&self, snapshot: &std::sync::Arc<RunSnapshot>) -> bool {
        let refreshed = self.refreshed.lock().unwrap_or_else(|p| p.into_inner());
        refreshed.is_none_or(|(at, seen)| {
            let age = at.elapsed();
            age >= BYPASS_DNS_TTL || (seen != snapshot_key(snapshot) && age >= BYPASS_POLL_INTERVAL)
        })
    }

    /// Re-compose the list from `snapshot` and re-resolve it (blocking:
    /// DNS). `Some(config)` when the list changed — the caller pushes it to
    /// every live Shim.
    pub(crate) fn refresh(
        &self,
        snapshot: &std::sync::Arc<RunSnapshot>,
    ) -> Option<crate::shim::proto::BypassConfig> {
        let endpoints = compose_provider_hosts(snapshot);
        self.refresh_with(&endpoints, snapshot_key(snapshot), None)
    }

    /// [`Self::refresh`] over given endpoints, with an optional lookup (the
    /// test seam).
    pub(crate) fn refresh_with(
        &self,
        endpoints: &[Endpoint],
        seen: usize,
        lookup: Option<Lookup<'_>>,
    ) -> Option<crate::shim::proto::BypassConfig> {
        let resolved = match lookup {
            Some(lookup) => self.resolver.resolve_with(endpoints, lookup),
            None => self.resolver.resolve(endpoints),
        };
        *self.refreshed.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), seen));
        {
            let mut list = self.list.lock().unwrap_or_else(|p| p.into_inner());
            if *list == resolved {
                return None;
            }
            *list = resolved;
        }
        self.config()
    }
}

/// A resolver the tests substitute for `getaddrinfo`.
pub(crate) type Lookup<'a> = &'a dyn Fn(&str) -> Result<Vec<IpAddr>, String>;

/// A published snapshot's identity (its address) for the change check.
fn snapshot_key(snapshot: &std::sync::Arc<RunSnapshot>) -> usize {
    std::sync::Arc::as_ptr(snapshot) as usize
}

/// The listed hosts any of `seen` belongs to: a connection to one of a
/// host's addresses on its port (a shared address names every such host),
/// sorted.
pub fn matching_hosts(list: &[ProviderHost], seen: &[SocketAddr]) -> Vec<String> {
    let mut hosts: Vec<String> = list
        .iter()
        .filter(|entry| {
            seen.iter()
                .any(|addr| addr.port() == entry.port && entry.addrs.contains(&addr.ip()))
        })
        .map(|entry| entry.host.clone())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// What a finding means for a Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BypassVerdict {
    /// The Run's calls go through Hekma, yet its tree reached a provider
    /// directly: the `bypass` Anomaly.
    Anomaly,
    /// The Run has no wire path (its agent holds its own provider key):
    /// direct calls are expected.
    NoWire,
    /// Key custody is off for the Run: direct calls are expected.
    CustodyOff,
}

/// The verdict for a Run with `wire_path` and custody `custody_on`.
pub fn bypass_verdict(wire_path: bool, custody_on: Option<bool>) -> BypassVerdict {
    match (wire_path, custody_on) {
        (false, _) => BypassVerdict::NoWire,
        (true, Some(false)) => BypassVerdict::CustodyOff,
        (true, _) => BypassVerdict::Anomaly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feed_reports_nothing_until_a_list_and_pushes_only_a_change() {
        let feed = BypassFeed::default();
        let snapshot = std::sync::Arc::new(RunSnapshot::default());
        assert!(feed.refresh_due(&snapshot));
        assert!(
            feed.config().is_none(),
            "no list yet: no detection, never a clean read"
        );
        let at = |host: &str, port| Endpoint {
            host: host.to_string(),
            port,
        };
        let lookup = |_: &str| Ok(vec!["10.1.2.3".parse().unwrap()]);
        let first = feed
            .refresh_with(
                &[at("api.x.com", 443), at("127.0.0.1", 9000)],
                snapshot_key(&snapshot),
                Some(&lookup),
            )
            .expect("the first list is a change");
        assert_eq!(first.interval_ms, 5_000);
        assert_eq!(first.hosts.len(), 2);
        assert!(!feed.refresh_due(&snapshot));
        // A new snapshot is due only a poll interval after the last
        // composition (never a walk per tick).
        let newer = std::sync::Arc::new(RunSnapshot::default());
        assert!(!feed.refresh_due(&newer));
        assert_eq!(
            feed.listed().into_iter().collect::<Vec<_>>(),
            vec!["127.0.0.1", "api.x.com"]
        );
        assert!(
            feed.refresh_with(
                &[at("api.x.com", 443), at("127.0.0.1", 9000)],
                snapshot_key(&snapshot),
                Some(&lookup)
            )
            .is_none(),
            "an unchanged list is not pushed again"
        );
        let changed =
            feed.refresh_with(&[at("api.y.com", 443)], snapshot_key(&newer), Some(&lookup));
        assert_eq!(
            changed.expect("a changed list is pushed").hosts[0].host,
            "api.y.com"
        );
    }

    #[test]
    fn hosts_come_out_of_urls_and_origins() {
        assert_eq!(
            host_of("https://api.x.com:443/v1").as_deref(),
            Some("api.x.com")
        );
        assert_eq!(
            host_of("http://127.0.0.1:9/v1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(host_of("https://[::1]:8443").as_deref(), Some("::1"));
        assert_eq!(
            host_of("https://u:p@API.X.com").as_deref(),
            Some("api.x.com")
        );
        assert_eq!(host_of("api.y.com").as_deref(), Some("api.y.com"));
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn the_list_holds_the_builtins_and_every_configured_origin() {
        let snapshot = RunSnapshot::default();
        let hosts = compose_provider_hosts(&snapshot);
        let openai = Endpoint {
            host: "api.openai.com".to_string(),
            port: 443,
        };
        assert!(hosts.contains(&openai));
        assert!(hosts.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
        assert_eq!(
            endpoint_of("http://127.0.0.1:9000/v1"),
            Some(Endpoint {
                host: "127.0.0.1".into(),
                port: 9000
            })
        );
        assert_eq!(endpoint_of("http://gw.local").map(|e| e.port), Some(80));
        assert_eq!(
            endpoint_of("https://[::1]:8443").map(|e| e.port),
            Some(8443)
        );
        assert_eq!(endpoint_of("https://[::1]").map(|e| e.port), Some(443));
        assert_eq!(endpoint_of("https://"), None);
    }

    #[test]
    fn resolution_caches_keeps_the_last_good_set_and_takes_literals_as_is() {
        let resolver = HostResolver::default();
        let ok = |_: &str| {
            Ok(vec![
                "10.0.0.2".parse().unwrap(),
                "10.0.0.1".parse().unwrap(),
            ])
        };
        let at = |host: &str| Endpoint {
            host: host.to_string(),
            port: 443,
        };
        let hosts = vec![at("api.x.com"), at("127.0.0.1")];
        let first = resolver.resolve_with(&hosts, &ok);
        assert_eq!(first[0].addrs.len(), 2);
        assert_eq!(first[0].addrs[0], "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(first[1].addrs, vec!["127.0.0.1".parse::<IpAddr>().unwrap()]);
        // Fresh: the lookup is not called again.
        let failing = |_: &str| Err("no".to_string());
        let again = resolver.resolve_with(&hosts, &failing);
        assert_eq!(again[0].addrs.len(), 2);
        // A host that never resolved has no addresses.
        let unknown = resolver.resolve_with(&[at("nope.invalid")], &failing);
        assert!(unknown[0].addrs.is_empty());
        let real = resolver.resolve(&[at("localhost")]);
        assert_eq!(real[0].host, "localhost");
    }

    #[test]
    fn a_seen_address_names_every_host_it_belongs_to() {
        let shared: IpAddr = "10.0.0.9".parse().unwrap();
        let entry = |host: &str, addr: IpAddr| ProviderHost {
            host: host.into(),
            port: 443,
            addrs: vec![addr],
        };
        let list = vec![
            entry("b.com", shared),
            entry("a.com", shared),
            entry("c.com", "10.0.0.3".parse().unwrap()),
        ];
        let seen = SocketAddr::new(shared, 443);
        assert_eq!(matching_hosts(&list, &[seen]), vec!["a.com", "b.com"]);
        // The same address on another port (the metering proxy beside a
        // loopback gateway) is not the listed endpoint.
        assert!(matching_hosts(&list, &[SocketAddr::new(shared, 8400)]).is_empty());
        assert!(matching_hosts(&list, &["8.8.8.8:443".parse().unwrap()]).is_empty());
    }

    #[test]
    fn the_verdict_reads_the_wire_path_and_custody() {
        assert_eq!(bypass_verdict(true, Some(true)), BypassVerdict::Anomaly);
        assert_eq!(bypass_verdict(true, None), BypassVerdict::Anomaly);
        assert_eq!(bypass_verdict(true, Some(false)), BypassVerdict::CustodyOff);
        assert_eq!(bypass_verdict(false, Some(true)), BypassVerdict::NoWire);
    }
}
