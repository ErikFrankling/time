//! Where Erik is, from which network his devices are on. No GPS, no
//! location permission for this: a Wi-Fi name, a router address or a DNS
//! search domain is enough to tell home from KTH.

use serde::Deserialize;

use crate::event::Network;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceSpec {
    pub name: String,
    /// Wi-Fi names; `*` wildcards, case-insensitive.
    #[serde(default)]
    pub ssids: Vec<String>,
    /// Default-gateway addresses (the router).
    #[serde(default)]
    pub gateways: Vec<String>,
    /// DNS search domains; also matches subdomains.
    #[serde(default)]
    pub domains: Vec<String>,
}

/// The places used when the config names none.
pub fn defaults() -> Vec<PlaceSpec> {
    vec![
        PlaceSpec {
            name: "home".into(),
            ssids: vec![],
            gateways: vec!["192.168.50.1".into()],
            domains: vec![],
        },
        PlaceSpec {
            name: "KTH".into(),
            ssids: vec!["eduroam".into(), "KTH*".into()],
            domains: vec!["kth.se".into()],
            gateways: vec![],
        },
    ]
}

fn glob(p: &str, s: &str) -> bool {
    let (p, s) = (p.to_lowercase(), s.to_lowercase());
    match p.split_once('*') {
        None => p == s,
        Some((a, b)) => {
            s.starts_with(a) && s[a.len()..].ends_with(b) && s.len() >= a.len() + b.len()
        }
    }
}

/// The named place a network belongs to; otherwise "Wi-Fi <name>" when the
/// name is known; None when nothing identifies it.
pub fn resolve(places: &[PlaceSpec], n: &Network) -> Option<String> {
    for p in places {
        let ssid = n
            .ssid
            .as_deref()
            .is_some_and(|s| p.ssids.iter().any(|g| glob(g, s)));
        let gw = n
            .gateway
            .as_deref()
            .is_some_and(|g| p.gateways.iter().any(|x| x == g));
        let dom = n.domain.as_deref().is_some_and(|d| {
            let d = d.to_lowercase();
            p.domains
                .iter()
                .any(|x| d == *x || d.ends_with(&format!(".{x}")))
        });
        if ssid || gw || dom {
            return Some(p.name.clone());
        }
    }
    n.ssid
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("Wi-Fi {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ssid: Option<&str>, gateway: Option<&str>, domain: Option<&str>) -> Network {
        Network {
            ssid: ssid.map(Into::into),
            gateway: gateway.map(Into::into),
            domain: domain.map(Into::into),
        }
    }

    #[test]
    fn home_by_router_kth_by_eduroam_or_domain_else_the_ssid() {
        let p = defaults();
        assert_eq!(
            resolve(&p, &net(None, Some("192.168.50.1"), None)).as_deref(),
            Some("home")
        );
        assert_eq!(
            resolve(&p, &net(Some("eduroam"), Some("10.1.2.1"), None)).as_deref(),
            Some("KTH")
        );
        assert_eq!(
            resolve(&p, &net(None, None, Some("wlan.kth.se"))).as_deref(),
            Some("KTH")
        );
        assert_eq!(
            resolve(&p, &net(Some("Espresso House"), None, None)).as_deref(),
            Some("Wi-Fi Espresso House")
        );
        assert_eq!(resolve(&p, &net(None, Some("10.0.0.1"), None)), None);
    }
}
