//! Which addresses the gateway may connect to for upstream providers and OIDC.
//!
//! Admins configure upstream URLs, so without a guard an admin account (or a stolen admin
//! session) could make the gateway call cloud metadata services or other internal endpoints and
//! read the answer back through error messages. Two layers enforce the policy:
//!
//! - a DNS resolver for the HTTP clients (the gateway's own and the AWS SDK's, for Bedrock) that
//!   drops blocked addresses, so a hostname cannot resolve (or re-resolve, DNS rebinding) to an
//!   internal address;
//! - [`Policy::check_url`] for URLs with an IP literal host, which skip DNS resolution.
//!
//! Redirects are not followed by the clients that use this policy, so a public endpoint cannot
//! redirect into the internal network either.
//!
//! Cloud metadata and host-channel addresses are always blocked. Loopback and private networks
//! (RFC 1918, carrier-grade NAT, IPv6 unique local) are blocked unless listed in
//! `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`, for self-hosted models, private endpoints and VPC endpoints.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

/// An IP network in CIDR notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    addr: IpAddr,
    prefix: u8,
}

impl Network {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| format!("'{s}' is not an IP network"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(|| format!("'{s}' has an invalid prefix length"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => prefix_match(&n.octets(), &a.octets(), self.prefix),
            (IpAddr::V6(n), IpAddr::V6(a)) => prefix_match(&n.octets(), &a.octets(), self.prefix),
            _ => false,
        }
    }
}

fn prefix_match(net: &[u8], addr: &[u8], prefix: u8) -> bool {
    let (full, rest) = ((prefix / 8) as usize, prefix % 8);
    if net[..full] != addr[..full] {
        return false;
    }
    rest == 0 || {
        let mask = 0xffu8 << (8 - rest);
        net[full] & mask == addr[full] & mask
    }
}

/// Where the gateway may open outbound connections.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Loopback and private networks that are allowed anyway.
    allowed: Vec<Network>,
}

impl Policy {
    /// `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`: comma-separated CIDR networks.
    pub fn from_list(list: &str) -> Result<Self, String> {
        let allowed = list.split(',').filter(|s| !s.trim().is_empty()).map(Network::parse).collect::<Result<_, _>>()?;
        Ok(Self { allowed })
    }

    /// The networks allowed in addition to public addresses, as CIDR.
    pub fn networks(&self) -> Vec<String> {
        self.allowed.iter().map(|n| format!("{}/{}", n.addr, n.prefix)).collect()
    }

    /// Why a connection to `ip` is not allowed, or `None` when it is.
    pub fn deny_reason(&self, ip: IpAddr) -> Option<&'static str> {
        let ip = canonical(ip);
        if is_metadata(ip) {
            return Some("a cloud metadata or link-local address");
        }
        if is_unroutable(ip) {
            return Some("an unspecified, broadcast or multicast address");
        }
        if is_internal(ip) && !self.allowed.iter().any(|n| n.contains(ip)) {
            return Some("a loopback or private address (allow it with GATEWAY_UPSTREAM_ALLOWED_NETWORKS)");
        }
        None
    }

    /// Checks a URL whose host is an IP literal; hostnames are checked when they are resolved.
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        let parsed = reqwest::Url::parse(url).map_err(|_| "not a valid URL".to_string())?;
        // The URL parser normalizes IPv4 hosts (decimal, hex, short forms) to dotted quads.
        let host = parsed.host_str().ok_or_else(|| "the URL has no host".to_string())?;
        let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() else {
            return Ok(());
        };
        match self.deny_reason(ip) {
            Some(why) => Err(format!("{ip} is {why}")),
            None => Ok(()),
        }
    }

    /// A DNS resolver for reqwest that drops addresses this policy blocks.
    pub fn resolver(&self) -> Arc<GuardedResolver> {
        Arc::new(GuardedResolver { policy: self.clone() })
    }
}

/// IPv4-mapped and NAT64 IPv6 addresses are judged by the IPv4 address they carry.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let o = v6.octets();
                return IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            ip
        }
        v4 => v4,
    }
}

/// Instance metadata and host channels of the major clouds; never an upstream.
fn is_metadata(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_link_local() // 169.254.0.0/16: AWS, GCP, Azure and OCI IMDS, ECS task metadata
                || v4 == Ipv4Addr::new(100, 100, 100, 200) // Alibaba Cloud
                || v4 == Ipv4Addr::new(168, 63, 129, 16) // Azure WireServer
                || v4 == Ipv4Addr::new(192, 0, 0, 192) // Oracle Cloud
        }
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
                || v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) // AWS IMDS over IPv6
        }
    }
}

fn is_unroutable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 0 || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || v4.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfec0 // ULA, site-local
        }
    }
}

pub struct GuardedResolver {
    policy: Policy,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let allowed = lookup(&policy, &host).await?;
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The same guard as a DNS resolver for the AWS SDK's HTTP client, which Bedrock calls go through.
#[derive(Debug, Clone)]
pub struct SdkResolver(pub Policy);

impl aws_smithy_runtime_api::client::dns::ResolveDns for SdkResolver {
    fn resolve_dns<'a>(&'a self, name: &'a str) -> aws_smithy_runtime_api::client::dns::DnsFuture<'a> {
        aws_smithy_runtime_api::client::dns::DnsFuture::new(async move {
            let allowed = lookup(&self.0, name).await.map_err(aws_smithy_runtime_api::client::dns::ResolveDnsError::new)?;
            Ok(allowed.into_iter().map(|a| a.ip()).collect())
        })
    }
}

/// Resolve `host` and keep the addresses the policy allows; none allowed is an error.
async fn lookup(policy: &Policy, host: &str) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
    let mut reason = None;
    let allowed: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|a| match policy.deny_reason(a.ip()) {
            Some(why) => {
                reason.get_or_insert((a.ip(), why));
                false
            }
            None => true,
        })
        .collect();
    if allowed.is_empty() {
        let msg = match reason {
            Some((ip, why)) => format!("{host} resolves to {ip}, which is {why}"),
            None => format!("{host} did not resolve"),
        };
        tracing::warn!(host = %host, "blocked outbound connection: {msg}");
        return Err(msg.into());
    }
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_policy() {
        let p = Policy::default();
        for blocked in [
            "169.254.169.254",
            "169.254.170.2",
            "fd00:ec2::254",
            "fe80::1",
            "100.100.100.200",
            "168.63.129.16",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fd12::1",
            "0.0.0.0",
            "255.255.255.255",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(p.deny_reason(ip(blocked)).is_some(), "{blocked} should be blocked");
        }
        for allowed in ["52.94.1.1", "20.42.0.1", "2606:4700::1", "8.8.8.8", "::ffff:8.8.8.8"] {
            assert!(p.deny_reason(ip(allowed)).is_none(), "{allowed} should be allowed");
        }
    }

    #[test]
    fn allowed_networks() {
        let p = Policy::from_list("10.20.0.0/16, 127.0.0.1, ::1/128").unwrap();
        assert!(p.deny_reason(ip("10.20.5.5")).is_none());
        assert!(p.deny_reason(ip("10.21.0.1")).is_some());
        assert!(p.deny_reason(ip("127.0.0.1")).is_none());
        assert!(p.deny_reason(ip("127.0.0.2")).is_some());
        assert!(p.deny_reason(ip("::1")).is_none());
        // Metadata stays blocked even when an operator lists it.
        let wide = Policy::from_list("0.0.0.0/0").unwrap();
        assert!(wide.deny_reason(ip("169.254.169.254")).is_some());
        assert!(wide.deny_reason(ip("192.168.0.10")).is_none());
        assert!(Policy::from_list("10.0.0.0/33").is_err());
        assert!(Policy::from_list("nope").is_err());
    }

    /// The resolver as reqwest uses it: `localhost` resolves (without network) to loopback.
    #[tokio::test]
    async fn resolver_blocks_names_that_resolve_internally() {
        let client = |p: &Policy| reqwest::Client::builder().dns_resolver(p.resolver()).build().unwrap();
        let err = client(&Policy::default()).get("http://localhost:9/").send().await.unwrap_err();
        let chain = crate::util::error_chain(&err);
        assert!(chain.contains("loopback or private address"), "{chain}");
        // Allowed: the name resolves, and the request fails only because nothing listens on port 9.
        let allowed = Policy::from_list("127.0.0.0/8,::1").unwrap();
        let err = client(&allowed).get("http://localhost:9/").send().await.unwrap_err();
        assert!(!crate::util::error_chain(&err).contains("loopback or private address"));
    }

    #[test]
    fn literal_urls() {
        let p = Policy::default();
        assert!(p.check_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(p.check_url("http://[fd00:ec2::254]/").is_err());
        assert!(p.check_url("http://127.0.0.1:9999/v1").is_err());
        assert!(p.check_url("https://api.openai.com/v1").is_ok());
        assert!(p.check_url("https://52.94.1.1/").is_ok());
        // Alternative IPv4 spellings are normalized before the check.
        assert!(p.check_url("http://2852039166/").is_err());
        assert!(p.check_url("http://0xa9.0xfe.0xa9.0xfe/").is_err());
    }

    #[tokio::test]
    async fn sdk_resolver_applies_the_policy() {
        use aws_smithy_runtime_api::client::dns::ResolveDns;
        assert!(SdkResolver(Policy::default()).resolve_dns("localhost").await.is_err(), "loopback is blocked by default");
        let allowed = SdkResolver(Policy::from_list("127.0.0.0/8,::1/128").unwrap()).resolve_dns("localhost").await.unwrap();
        assert!(allowed.iter().all(|ip| ip.is_loopback()) && !allowed.is_empty());
    }
}
