//! Which peers are reverse proxies the host believes about who the client
//! is. The host sits behind one (Caddy on the same machine, or a load
//! balancer on another), so the peer of a connection is the proxy; the client
//! is what the proxy wrote in `X-Forwarded-For`. Anyone can send that header,
//! so it counts only on a connection from a proxy the operator listed, or
//! from this machine.
use std::net::IpAddr;

/// The operator setting: addresses and networks, separated by commas or
/// spaces, such as `10.0.0.5, 10.1.0.0/16, fd00:1::/64`. Empty by default:
/// only a proxy on this machine is believed.
pub const TRUSTED_PROXIES_KEY: &str = "session.trusted_proxies";
/// The most entries the setting may hold.
const MAX_ENTRIES: usize = 64;

/// The operator's list of trusted proxies ([`TRUSTED_PROXIES_KEY`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<(IpAddr, u8)>);
impl TrustedProxies {
    /// Reads the setting's text. `None` when an entry is not an address or
    /// a network, or names every address (`/0`).
    pub fn parse(list: &str) -> Option<Self> {
        let entries = list
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let (address, prefix) = match entry.split_once('/') {
                    Some((address, prefix)) => (address, Some(prefix)),
                    None => (entry, None),
                };
                let address = address.parse::<IpAddr>().ok()?.to_canonical();
                let whole = if address.is_ipv4() { 32 } else { 128 };
                let prefix = match prefix {
                    Some(prefix) => prefix.parse::<u8>().ok()?,
                    None => whole,
                };
                (1..=whole).contains(&prefix).then_some((address, prefix))
            })
            .collect::<Option<Vec<_>>>()?;
        (entries.len() <= MAX_ENTRIES).then_some(Self(entries))
    }

    /// Whether a connection from `peer` is a proxy's: this machine, or an
    /// address the operator listed.
    pub fn trusts(&self, peer: IpAddr) -> bool {
        let peer = peer.to_canonical();
        peer.is_loopback()
            || self
                .0
                .iter()
                .any(|(network, prefix)| match (network, peer) {
                    (IpAddr::V4(network), IpAddr::V4(peer)) => {
                        (u32::from(*network) ^ u32::from(peer)) >> (32 - prefix) == 0
                    }
                    (IpAddr::V6(network), IpAddr::V6(peer)) => {
                        (u128::from(*network) ^ u128::from(peer)) >> (128 - prefix) == 0
                    }
                    _ => false,
                })
    }

    /// The client of a connection from `peer` that carried these
    /// `X-Forwarded-For` hops, in the order they were sent. A peer that is
    /// not a trusted proxy is the client, whatever it sent. Otherwise the
    /// hops are read right to left, skipping trusted proxies, and the first
    /// other address is the client; an unreadable hop leaves the peer.
    pub fn client<'a>(
        &self,
        peer: IpAddr,
        forwarded: impl DoubleEndedIterator<Item = &'a str>,
    ) -> IpAddr {
        if !self.trusts(peer) {
            return peer;
        }
        for hop in forwarded.rev() {
            match hop.trim().parse::<IpAddr>() {
                Ok(address) if self.trusts(address) => continue,
                Ok(address) => return address,
                Err(_) => break,
            }
        }
        peer
    }
}
