//! This module defines and handles the Prometheus probe target models and resolution.
//!
//! Therefore, this includes the necessary logic to deserialize, validate and convert the supplied information as well
//! as resolve the real target address. The conversion is based on the defaults of Minecraft, and therefore relevant
//! default values and SRV records are considered while resolving the dynamic target address. It is the responsibility
//! of this module to standardize the desired probing that should be performed for a request.

use hickory_resolver::proto::rr::RecordType::SRV;
use hickory_resolver::{ResolveError, TokioResolver};
use serde::de::{Unexpected, Visitor};
use serde::{Deserialize, Deserializer, de};
use std::fmt;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use tracing::log::debug;

/// The internal error type for all [`ProbingInfo`] related errors.
///
/// This includes errors with the resolution of the [`TargetAddress`] or any other errors that may occur while trying to
/// make sense of the supplied probe target information. Any [`Error`] results in mcexport not being able to perform
/// any ping of the desired probe.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The resolution failed because there was a communication error with the responsible DNS name server.
    #[error("failed to resolve a DNS record: {0}")]
    ResolutionFail(#[from] ResolveError),
    /// The resolution failed because no valid A record was specified for the supplied (or configured) hostname.
    #[error("could not resolve any \"A\" DNS entries for hostname \"{0}\"")]
    CouldNotResolveIp(String),
}

/// `ResolutionResult` is the outcome of a DNS resolution for a supplied [`TargetAddress`].
///
/// The result holds the final resolved [`SocketAddr`] of a supplied target [`TargetAddress`]. It is differentiated
/// between the different ways to retrieve the final address. A [`Plain`][plain] resolution, where no SRV record was
/// found and the supplied hostname was directly resolved to the final IP-Address and an [`Srv`][srv] resolution that
/// was performed on the indirect hostname, resolved through the corresponding SRV record.
///
/// [plain]: ResolutionResult::Plain
/// [srv]: ResolutionResult::Srv
#[derive(Debug, PartialEq, Eq)]
pub enum ResolutionResult {
    /// There was an SRV record and the resolved IP address is of the target hostname within this record.
    Srv(SocketAddr),
    /// There was no SRV record and the resolved IP address is of the original hostname.
    Plain(SocketAddr),
}

/// `ProbingInfo` is the information supplied by Prometheus for each probe request.
///
/// This information is supplied for each probe request and needs to be used to ping the right target. We cannot handle
/// requests that come without this query information. While the target address is mandatory, the module is completely
/// optional and not required for the correct operation of mcexport.
#[derive(Debug, Deserialize, Clone)]
pub struct ProbingInfo {
    /// The target that mcexport should ping for the corresponding probe request.
    pub target: TargetAddress,
    /// The module that mcexport should use to ping for the corresponding probe request.
    pub module: Option<String>,
}

impl Display for ProbingInfo {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match &self.module {
            Some(module) => write!(formatter, "{} ({})", self.target, module),
            _ => write!(formatter, "{}", self.target),
        }
    }
}

/// `TargetAddress` is the combination of a hostname or textual IP address with a port.
///
/// This address should be used to issue a probing ping. The information comes from the request of Prometheus and needs
/// to be validated and parsed before it can be used. To get the real target address, the hostname needs to be resolved
/// and the corresponding SRV records need to be considered.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TargetAddress {
    /// The hostname that should be resolved to the IP address (optionally considering SRV records).
    pub hostname: String,
    /// The post that should be used to ping the Minecraft server (ignored if SRV exists).
    pub port: u16,
}

impl TargetAddress {
    /// Converts this hostname and port to the resolved [socket address][SocketAddr].
    ///
    /// The hostname of this address is resolved through DNS and is used as the IP address of the resulting socket
    /// address. To do this, we also check the SRV record for minecraft (`_minecraft._tcp`) and prefer to use this
    /// information. If any SRV record was found, the second return of this function will be true.
    pub async fn to_socket_addrs(
        &self,
        resolver: &TokioResolver,
    ) -> Result<ResolutionResult, Error> {
        // assemble the SRV record name
        let srv_name = format!("_minecraft._tcp.{}", self.hostname);
        debug!("trying to resolve SRV record: '{srv_name}'");
        let srv_response = resolver.lookup(&srv_name, SRV).await;

        // check if any SRV record was present (use this data then)
        let (hostname, port, srv_used) = srv_response.map_or_else(
            |_| {
                debug!("found no SRV record for '{srv_name}'");
                (self.hostname.clone(), self.port, false)
            },
            |response| {
                response.iter().find_map(|rec| rec.as_srv()).map_or_else(
                    || {
                        debug!(
                            "found an SRV record for '{srv_name}', but it was of an invalid type"
                        );
                        (self.hostname.clone(), self.port, false)
                    },
                    |record| {
                        let target = record.target().to_utf8();
                        let target_port = record.port();
                        debug!("found an SRV record for '{srv_name}': {target}:{target_port}");
                        (target, target_port, true)
                    },
                )
            },
        );

        // resolve the underlying ips for the hostname
        let ip_response = resolver.lookup_ip(&hostname).await?;
        for ip in ip_response {
            debug!("resolved ip address {} for hostname {}", ip, &hostname);
            if ip.is_ipv4() {
                return if srv_used {
                    Ok(ResolutionResult::Srv(SocketAddr::new(ip, port)))
                } else {
                    Ok(ResolutionResult::Plain(SocketAddr::new(ip, port)))
                };
            }
        }

        // no IPv4 could be found, return an error
        Err(Error::CouldNotResolveIp(hostname))
    }
}

impl Display for TargetAddress {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.hostname, self.port)
    }
}

/// The visitor for the deserialization of [`TargetAddress`].
///
/// This visitor is responsible for the deserialization and validation of a [`TargetAddress`] and returns the
/// appropriate expectations of the format. The address is expected in the format of `hostname:port` and the port is
/// optional, falling back to the default port.
struct ProbeAddressVisitor;

impl Visitor<'_> for ProbeAddressVisitor {
    type Value = TargetAddress;

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("a string in the form 'hostname' or 'hostname:port'")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        // split the supplied hostname and port into their own parts
        let mut parts = value.splitn(2, ':');

        // get the hostname and port part in their raw form
        let hostname = parts
            .next()
            .expect("splitting a string should result in at least one element");
        let port_str = parts.next().unwrap_or("25565");

        // parse the port into the expected form
        let port: u16 = port_str.parse().map_err(|_| {
            de::Error::invalid_value(Unexpected::Str(port_str), &"a valid port number")
        })?;

        // check if the hostname is present
        if hostname.is_empty() {
            return Err(de::Error::invalid_value(
                Unexpected::Str(hostname),
                &"a non-empty hostname",
            ));
        }

        // check if the port is valid
        if port == 0 {
            return Err(de::Error::invalid_value(
                Unexpected::Unsigned(port.into()),
                &"a positive port number",
            ));
        }

        // wrap the parsed parts into our ProbeAddress
        Ok(TargetAddress {
            hostname: hostname.to_owned(),
            port,
        })
    }
}

impl<'de> Deserialize<'de> for TargetAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ProbeAddressVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::Resolver;
    use serde_test::{Token, assert_de_tokens, assert_de_tokens_error};

    #[test]
    fn deserialize_without_port() {
        let address = TargetAddress {
            hostname: "mc.justchunks.net".to_string(),
            port: 25565,
        };

        assert_de_tokens(&address, &[Token::Str("mc.justchunks.net")])
    }

    #[test]
    fn deserialize_without_port_ip() {
        let address = TargetAddress {
            hostname: "123.123.123.123".to_string(),
            port: 25565,
        };

        assert_de_tokens(&address, &[Token::Str("123.123.123.123")])
    }

    #[test]
    fn deserialize_with_port() {
        let address = TargetAddress {
            hostname: "mc.justchunks.net".to_string(),
            port: 25566,
        };

        assert_de_tokens(&address, &[Token::Str("mc.justchunks.net:25566")])
    }

    #[test]
    fn deserialize_with_port_ip() {
        let address = TargetAddress {
            hostname: "123.123.123.123".to_string(),
            port: 25566,
        };

        assert_de_tokens(&address, &[Token::Str("123.123.123.123:25566")])
    }

    #[test]
    fn fail_deserialize_with_extra_colons() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("mc.justchunks.net:test:25566")],
            "invalid value: string \"test:25566\", expected a valid port number",
        )
    }

    #[test]
    fn fail_deserialize_with_port_negative() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("mc.justchunks.net:-5")],
            "invalid value: string \"-5\", expected a valid port number",
        )
    }

    #[test]
    fn fail_deserialize_with_port_too_low() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("mc.justchunks.net:0")],
            "invalid value: integer `0`, expected a positive port number",
        )
    }

    #[test]
    fn fail_deserialize_with_port_too_high() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("mc.justchunks.net:100000")],
            "invalid value: string \"100000\", expected a valid port number",
        )
    }

    #[test]
    fn fail_deserialize_with_port_non_numeric() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("mc.justchunks.net:text")],
            "invalid value: string \"text\", expected a valid port number",
        )
    }

    #[test]
    fn fail_deserialize_with_empty() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str("")],
            "invalid value: string \"\", expected a non-empty hostname",
        )
    }

    #[test]
    fn fail_deserialize_with_empty_hostname() {
        assert_de_tokens_error::<TargetAddress>(
            &[Token::Str(":25566")],
            "invalid value: string \"\", expected a non-empty hostname",
        )
    }

    #[tokio::test]
    async fn resolve_real_address_with_srv() {
        let resolver = Resolver::builder_tokio().unwrap().build();
        let probe_address = TargetAddress {
            hostname: "justchunks.net".to_string(),
            port: 1337,
        };
        let resolution_result = probe_address.to_socket_addrs(&resolver).await.unwrap();
        let expected_address = SocketAddr::from(([142, 132, 245, 251], 25565));

        assert_eq!(resolution_result, ResolutionResult::Srv(expected_address));
    }

    #[tokio::test]
    async fn resolve_real_address_without_srv() {
        let resolver = Resolver::builder_tokio().unwrap().build();
        let probe_address = TargetAddress {
            hostname: "mc.justchunks.net".to_string(),
            port: 25566,
        };
        let resolution_result = probe_address.to_socket_addrs(&resolver).await.unwrap();
        let expected_address = SocketAddr::from(([142, 132, 245, 251], 25566));

        assert_eq!(resolution_result, ResolutionResult::Plain(expected_address));
    }

    #[tokio::test]
    async fn resolve_real_ip_address() {
        let resolver = Resolver::builder_tokio().unwrap().build();
        let probe_address = TargetAddress {
            hostname: "142.132.245.251".to_string(),
            port: 25566,
        };
        let resolution_result = probe_address.to_socket_addrs(&resolver).await.unwrap();
        let expected_address = SocketAddr::from(([142, 132, 245, 251], 25566));

        assert_eq!(resolution_result, ResolutionResult::Plain(expected_address));
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_resolve_illegal_address() {
        let resolver = Resolver::builder_tokio().unwrap().build();
        let probe_address = TargetAddress {
            hostname: "illegal_address".to_string(),
            port: 25566,
        };
        probe_address.to_socket_addrs(&resolver).await.unwrap();
    }

    #[tokio::test]
    #[should_panic]
    async fn fail_resolve_illegal_ip_address() {
        let resolver = Resolver::builder_tokio().unwrap().build();
        let probe_address = TargetAddress {
            hostname: "500.132.245.251".to_string(),
            port: 25566,
        };
        probe_address.to_socket_addrs(&resolver).await.unwrap();
    }
}
