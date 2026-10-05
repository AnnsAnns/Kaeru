//! The daemon bind policy (M6, ADR-030): localhost by default; an explicit
//! bind address must be loopback or a **tailnet** address — the Tailscale
//! CGNAT range `100.64.0.0/10` or the tailnet IPv6 ULA `fd7a:115e:a297::/48`
//! — never `0.0.0.0` or a public address. A non-loopback bind requires the
//! `X-Auth-Token` shared secret: fail closed when none is configured.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

/// Why a requested bind address was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// The value is not a `host:port` socket address.
    BadAddress(String),
    /// The address is neither loopback nor a tailnet address.
    NotPrivate(String),
    /// A tailnet bind without a configured auth token.
    NeedsToken(String),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAddress(value) => write!(
                f,
                "{value:?} is not a `host:port` address (e.g. 127.0.0.1:8080 or 100.x.y.z:8080)"
            ),
            Self::NotPrivate(value) => write!(
                f,
                "{value:?} is neither loopback nor a tailnet address (100.64.0.0/10 or \
                 fd7a:115e:a297::/48). The daemon is never exposed to the public internet \
                 (ADR-030); put it behind cloudflared instead"
            ),
            Self::NeedsToken(value) => write!(
                f,
                "binding {value:?} beyond localhost requires an auth token: set `auth_token` \
                 in the config first (ADR-030 — fail closed, so nobody on the tailnet can \
                 reach the API unauthenticated)"
            ),
        }
    }
}

impl std::error::Error for BindError {}

/// Resolve the address to bind. A `None`/blank `bind` (a `host:port` string)
/// means the default `127.0.0.1:port`. `auth_configured` is whether the
/// config holds a non-empty `auth_token`.
pub fn resolve_bind(
    bind: Option<&str>,
    port: u16,
    auth_configured: bool,
) -> Result<SocketAddr, BindError> {
    let raw = bind.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    };
    let addr: SocketAddr = raw
        .parse()
        .map_err(|_| BindError::BadAddress(raw.to_owned()))?;
    if addr.ip().is_loopback() {
        return Ok(addr);
    }
    if is_tailnet(addr.ip()) {
        return if auth_configured {
            Ok(addr)
        } else {
            Err(BindError::NeedsToken(raw.to_owned()))
        };
    }
    Err(BindError::NotPrivate(raw.to_owned()))
}

/// Tailscale address ranges: the CGNAT range `100.64.0.0/10` and the tailnet
/// IPv6 ULA `fd7a:115e:a297::/48`.
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115e && segments[2] == 0xa297
        }
    }
}

/// Short scope label for the startup banner.
pub fn scope_label(addr: &SocketAddr) -> &'static str {
    if addr.ip().is_loopback() {
        "localhost only"
    } else {
        "tailnet only (ADR-030)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(bind: Option<&str>, auth: bool) -> Result<SocketAddr, BindError> {
        resolve_bind(bind, 8080, auth)
    }

    #[test]
    fn default_is_localhost_with_config_port() {
        assert_eq!(
            resolve(None, false).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
        assert_eq!(
            resolve(Some(""), true).unwrap(),
            resolve(None, true).unwrap()
        );
        assert_eq!(
            resolve(Some("   "), false).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
    }

    #[test]
    fn explicit_loopback_needs_no_token() {
        assert_eq!(
            resolve(Some("127.0.0.1:9000"), false).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 9000))
        );
        assert_eq!(
            resolve(Some("[::1]:9000"), false).unwrap(),
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 9000))
        );
    }

    #[test]
    fn tailnet_v4_requires_a_token() {
        assert_eq!(
            resolve(Some("100.64.0.1:8080"), false),
            Err(BindError::NeedsToken("100.64.0.1:8080".into()))
        );
        assert_eq!(
            resolve(Some("100.64.0.1:8080"), true).unwrap(),
            SocketAddr::from(([100, 64, 0, 1], 8080))
        );
        // Top of the CGNAT /10 is still tailnet.
        assert!(resolve(Some("100.127.255.255:8080"), true).is_ok());
    }

    #[test]
    fn tailnet_v6_requires_a_token() {
        assert_eq!(
            resolve(Some("[fd7a:115e:a297::1]:8080"), false),
            Err(BindError::NeedsToken("[fd7a:115e:a297::1]:8080".into()))
        );
        assert!(resolve(Some("[fd7a:115e:a297::1]:8080"), true).is_ok());
        // Outside the /48: not a tailnet address.
        assert_eq!(
            resolve(Some("[fd7a:115e:a298::1]:8080"), true),
            Err(BindError::NotPrivate("[fd7a:115e:a298::1]:8080".into()))
        );
    }

    #[test]
    fn public_and_lan_addresses_are_refused() {
        for value in [
            "0.0.0.0:8080",
            "[::]:8080",
            "8.8.8.8:8080",
            "192.168.1.10:8080",
            "100.128.0.0:8080", // just outside 100.64.0.0/10
        ] {
            assert_eq!(
                resolve(Some(value), true),
                Err(BindError::NotPrivate(value.into())),
                "{value} must be refused"
            );
        }
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        assert_eq!(
            resolve(Some("not-an-address"), true),
            Err(BindError::BadAddress("not-an-address".into()))
        );
        assert!(matches!(
            resolve(Some("100.64.0.1"), true),
            Err(BindError::BadAddress(_))
        ));
    }

    #[test]
    fn error_messages_name_the_policy() {
        assert!(
            BindError::NeedsToken("x".into())
                .to_string()
                .contains("auth_token")
        );
        assert!(
            BindError::NotPrivate("x".into())
                .to_string()
                .contains("tailnet")
        );
    }

    #[test]
    fn scope_labels() {
        assert_eq!(
            scope_label(&SocketAddr::from(([127, 0, 0, 1], 8080))),
            "localhost only"
        );
        assert_eq!(
            scope_label(&SocketAddr::from(([100, 64, 0, 1], 8080))),
            "tailnet only (ADR-030)"
        );
    }
}
