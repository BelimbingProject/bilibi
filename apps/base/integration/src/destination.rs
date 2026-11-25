//! Where a connector's own socket may go. A mailbox server or a phone system
//! that a tenant configures is reached over a protocol of its own (IMAP, SMTP,
//! a manager interface), not HTTP, so `Outgoing::public_https` cannot guard
//! it. The operator's policy is two global settings; a connector loads it on
//! its own connection, resolves the configured host through it on the thread
//! that will connect, and connects only to the addresses it checked, so a name
//! cannot answer one address to the check and another to the connection.
use crate::contributions::{CONNECTOR_ALLOWED_DESTINATIONS, CONNECTOR_PRIVATE_NETWORKS};
use bilibi_database::{Error, Result};
use bilibi_settings::Settings;
use bilibi_tenancy::ReadClient;
use serde_json::Value;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

/// The code a connector reports for every destination it cannot use before
/// connecting: a name that gives no address, an address outside the policy, an
/// empty host or port 0. It is the same for every reason and decided before
/// anything is sent, so it says nothing about which internal names exist or
/// whether something answers at an address.
pub const DESTINATION_REFUSED: &str = "destination_refused";

/// The most entries `integration.connector_allowed_destinations` may hold.
const MAX_ALLOWED: usize = 200;

/// The most addresses of one name that are checked and tried: a name may not
/// make a connector check or dial an unbounded list.
const MAX_ADDRESSES: usize = 16;

/// A configured host and port that cannot be used. It carries no reason on
/// purpose: a name that gives no address and one that gives a forbidden
/// address are the same refusal, so a tenant cannot tell which internal names
/// exist. Report [`DESTINATION_REFUSED`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unusable;

/// The operator's connector destination policy, as it stood when loaded.
///
/// An address is admitted when it is public, when it is private and the
/// operator lets connectors reach private networks, or when an allowed entry
/// names it (or the host it was resolved from). Link-local, cloud metadata,
/// unspecified, multicast and broadcast addresses are refused even then.
#[derive(Clone, Debug)]
pub struct ConnectorPolicy {
    private_networks: bool,
    allowed: Vec<Allowed>,
}

/// A host and port the policy checked: every address the host gave was
/// admitted. Connect to these addresses, never to the name again.
#[derive(Clone)]
pub struct Destination {
    addresses: Vec<SocketAddr>,
}

impl ConnectorPolicy {
    /// The policy now, read on the caller's connection, so a caller holding
    /// a pooled connection takes no second one.
    pub async fn load(settings: &Settings, client: &dyn ReadClient) -> Result<Self> {
        let private_networks = settings
            .get_global(client, CONNECTOR_PRIVATE_NETWORKS)
            .await?
            .as_bool()
            .ok_or(Error::Invalid)?;
        let allowed = allowed_entries(
            &settings
                .get_global(client, CONNECTOR_ALLOWED_DESTINATIONS)
                .await?,
        )
        .ok_or(Error::Invalid)?;
        Ok(Self {
            private_networks,
            allowed,
        })
    }

    /// Resolves `host` and checks every address it gives (the first
    /// `MAX_ADDRESSES` distinct ones, the only ones a connection may use). One
    /// refused address refuses the whole destination, and so does a name that
    /// gives no address: both are the same [`Unusable`]. Blocking (it asks the
    /// system resolver): call it on the thread that connects, right before
    /// connecting.
    pub fn resolve(&self, host: &str, port: u16) -> std::result::Result<Destination, Unusable> {
        let host = host.strip_prefix('[').unwrap_or(host);
        let host = host.strip_suffix(']').unwrap_or(host);
        if host.is_empty() || port == 0 {
            return Err(Unusable);
        }
        let mut addresses: Vec<SocketAddr> = Vec::new();
        for address in (host, port).to_socket_addrs().map_err(|_| Unusable)? {
            if addresses.len() == MAX_ADDRESSES {
                break;
            }
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        let named = self.names(host, port);
        if !addresses.is_empty()
            && addresses
                .iter()
                .all(|address| self.admitted(named, *address))
        {
            Ok(Destination { addresses })
        } else {
            Err(Unusable)
        }
    }

    /// Whether a connection already open to `address`, made for `host`, is
    /// still allowed. A connector that keeps a connection between uses asks
    /// before each use, so a policy the operator tightened ends it.
    pub fn admits(&self, host: &str, address: SocketAddr) -> bool {
        self.admitted(self.names(host, address.port()), address)
    }

    fn names(&self, host: &str, port: u16) -> bool {
        let host = host.strip_prefix('[').unwrap_or(host);
        let host = host.strip_suffix(']').unwrap_or(host);
        self.allowed.iter().any(|entry| entry.names(host, port))
    }

    fn admitted(&self, named: bool, address: SocketAddr) -> bool {
        let ip = address.ip().to_canonical();
        if never(ip) {
            return false;
        }
        named
            || self
                .allowed
                .iter()
                .any(|entry| entry.covers(ip, address.port()))
            || crate::transport::public_address(ip)
            || (self.private_networks && private(ip))
    }
}

impl Destination {
    /// The checked addresses, in the resolver's order.
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// Connects to the first checked address that accepts, all within one
    /// `timeout` in total, however many addresses there are. Blocking. A host
    /// may list an address family its server does not listen on first; only
    /// when none accepts in time does this fail.
    pub fn connect(&self, timeout: Duration) -> io::Result<TcpStream> {
        let deadline = Instant::now() + timeout;
        let mut last = io::Error::from(io::ErrorKind::AddrNotAvailable);
        for address in &self.addresses {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            match TcpStream::connect_timeout(address, left) {
                Ok(stream) => return Ok(stream),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

/// Never a connector's destination, whatever the operator allows: no mail or
/// phone server lives there, and a cloud instance's metadata service does.
fn never(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.octets()[0] == 0
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                // Alibaba Cloud's and Oracle Cloud's metadata services.
                || ip == Ipv4Addr::new(100, 100, 100, 200)
                || ip == Ipv4Addr::new(192, 0, 0, 192)
        }
        IpAddr::V6(ip) => {
            ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unicast_link_local()
                // Amazon EC2's metadata service over IPv6.
                || ip == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254)
        }
    }
}

/// The ranges `integration.connector_private_networks` opens: RFC 1918 and
/// IPv6 unique local addresses.
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

/// One entry of `integration.connector_allowed_destinations`: a host name or
/// an address block, and the port it is allowed on (any port when absent).
#[derive(Clone, Debug)]
struct Allowed {
    host: Host,
    port: Option<u16>,
}

#[derive(Clone, Debug)]
enum Host {
    /// Lower case, without a trailing dot.
    Name(String),
    Block(IpAddr, u8),
}

impl Allowed {
    fn names(&self, host: &str, port: u16) -> bool {
        self.port.is_none_or(|allowed| allowed == port)
            && matches!(&self.host, Host::Name(name)
                if name.eq_ignore_ascii_case(host.strip_suffix('.').unwrap_or(host)))
    }

    fn covers(&self, ip: IpAddr, port: u16) -> bool {
        self.port.is_none_or(|allowed| allowed == port)
            && match self.host {
                Host::Block(IpAddr::V4(block), prefix) => matches!(ip, IpAddr::V4(ip)
                    if prefix_bits(u128::from(u32::from(ip)), 32, prefix)
                        == prefix_bits(u128::from(u32::from(block)), 32, prefix)),
                Host::Block(IpAddr::V6(block), prefix) => matches!(ip, IpAddr::V6(ip)
                    if prefix_bits(u128::from(ip), 128, prefix)
                        == prefix_bits(u128::from(block), 128, prefix)),
                Host::Name(_) => false,
            }
    }

    /// `host`, `host:port`, `a.b.c.d[/len][:port]` or `[v6[/len]][:port]`.
    fn parse(entry: &str) -> Option<Self> {
        if entry.is_empty() || entry.len() > 300 || entry.trim() != entry {
            return None;
        }
        let (host, port) = if let Some(rest) = entry.strip_prefix('[') {
            let (inside, after) = rest.split_once(']')?;
            let port = match after {
                "" => None,
                after => Some(port(after.strip_prefix(':')?)?),
            };
            let (address, prefix) = block(inside, 128)?;
            return matches!(address, IpAddr::V6(_)).then_some(Self {
                host: Host::Block(address, prefix),
                port,
            });
        } else {
            match entry.rsplit_once(':') {
                Some((host, value)) => (host, Some(port(value)?)),
                None => (entry, None),
            }
        };
        if host.contains(':') {
            return None;
        }
        if host.starts_with(|first: char| first.is_ascii_digit()) && !name_only(host) {
            let (address, prefix) = block(host, 32)?;
            return matches!(address, IpAddr::V4(_)).then_some(Self {
                host: Host::Block(address, prefix),
                port,
            });
        }
        let name = host.strip_suffix('.').unwrap_or(host);
        let valid = !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        valid.then(|| Self {
            host: Host::Name(name.to_ascii_lowercase()),
            port,
        })
    }
}

/// A name whose last label has a letter, such as `1password.example`: an
/// address never does.
fn name_only(host: &str) -> bool {
    host.rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().any(|byte| byte.is_ascii_alphabetic()))
}

/// A whole decimal number: no sign, no space, at most five digits.
fn number(value: &str) -> Option<u32> {
    (!value.is_empty() && value.len() <= 5 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn port(value: &str) -> Option<u16> {
    u16::try_from(number(value)?).ok().filter(|port| *port != 0)
}

/// An address with an optional `/prefix` of at most `bits`.
fn block(value: &str, bits: u8) -> Option<(IpAddr, u8)> {
    let (address, prefix) = match value.split_once('/') {
        Some((address, prefix)) => (
            address,
            u8::try_from(number(prefix)?)
                .ok()
                .filter(|prefix| *prefix <= bits)?,
        ),
        None => (value, bits),
    };
    Some((address.parse().ok()?, prefix))
}

/// The first `prefix` bits of an address `width` bits wide.
fn prefix_bits(value: u128, width: u32, prefix: u8) -> u128 {
    let prefix = u32::from(prefix).min(width);
    if prefix == 0 {
        0
    } else {
        value >> (width - prefix)
    }
}

/// The parsed allow list, or `None` when any entry is not one of the forms.
fn allowed_entries(value: &Value) -> Option<Vec<Allowed>> {
    let entries = value.as_array()?;
    if entries.len() > MAX_ALLOWED {
        return None;
    }
    entries
        .iter()
        .map(|entry| Allowed::parse(entry.as_str()?))
        .collect()
}

/// The setting's validator: what `load` reads is always parseable.
pub(crate) fn valid_allowed(value: &Value) -> bool {
    allowed_entries(value).is_some()
}
