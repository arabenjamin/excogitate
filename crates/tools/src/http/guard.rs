//! Which addresses a request may reach. The check runs after DNS, on the
//! addresses the connection will use, so a name can't resolve to something
//! internal between a check and the connect.

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::config::HttpConfigError;

/// The address ranges a request may and may not reach.
#[derive(Debug)]
pub(super) struct AddressPolicy {
    blocked: Vec<IpNet>,
    allowed: Vec<IpNet>,
}

fn parse_ranges(ranges: &[String]) -> Result<Vec<IpNet>, HttpConfigError> {
    ranges
        .iter()
        .map(|range| {
            range
                .parse()
                .map_err(|_| HttpConfigError::Range(range.clone()))
        })
        .collect()
}

impl AddressPolicy {
    pub(super) fn new(blocked: &[String], allowed: &[String]) -> Result<Self, HttpConfigError> {
        Ok(Self {
            blocked: parse_ranges(blocked)?,
            allowed: parse_ranges(allowed)?,
        })
    }

    /// True if a request may reach `address`. An IPv4 address written as
    /// IPv6 is judged as what it is.
    pub(super) fn permits(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        self.allowed.iter().any(|range| range.contains(&address))
            || !self.blocked.iter().any(|range| range.contains(&address))
    }
}

/// A name resolved to an address the runtime may not reach.
#[derive(Debug)]
pub(super) struct BlockedAddress {
    pub(super) host: String,
}

impl fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} resolves to a blocked address", self.host)
    }
}

impl Error for BlockedAddress {}

/// Resolves names, and refuses one if any of its addresses is blocked.
#[derive(Debug)]
pub(super) struct GuardedResolver(pub(super) Arc<AddressPolicy>);

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = Arc::clone(&self.0);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if found.iter().any(|address| !policy.permits(address.ip())) {
                return Err(Box::new(BlockedAddress { host }) as Box<dyn Error + Send + Sync>);
            }
            let addresses: Addrs = Box::new(found.into_iter());
            Ok(addresses)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpConfig;

    fn policy(allowed: &[&str]) -> AddressPolicy {
        let allowed: Vec<String> = allowed.iter().map(|range| (*range).to_owned()).collect();
        AddressPolicy::new(&HttpConfig::default().blocked_ranges, &allowed).unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn internal_addresses_are_blocked_in_every_spelling() {
        let policy = policy(&[]);
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "fe80::1",
            "fd00::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            assert!(!policy.permits(ip(address)), "{address} was permitted");
        }
    }

    #[test]
    fn public_addresses_are_permitted() {
        let policy = policy(&[]);
        for address in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(policy.permits(ip(address)), "{address} was blocked");
        }
    }

    #[test]
    fn the_allow_list_overrides_only_what_it_names() {
        let policy = policy(&["127.0.0.1/32"]);
        assert!(policy.permits(ip("127.0.0.1")));
        assert!(policy.permits(ip("::ffff:127.0.0.1")));
        assert!(!policy.permits(ip("127.0.0.2")));
    }

    #[test]
    fn a_bad_range_is_refused() {
        let error = AddressPolicy::new(&["nonsense".to_owned()], &[]).unwrap_err();
        assert_eq!(error, HttpConfigError::Range("nonsense".to_owned()));
    }
}
