//! The network capability's text: allow-list patterns, URLs, redirect targets, the request a
//! plugin encodes, and which addresses may be dialled (against a reference written from the IANA
//! special-purpose registries).
use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Addresses that are never a public peer, from the IANA IPv4 and IPv6 special-purpose registries
/// and the cloud metadata services: written out again, apart from `classify`.
fn reference_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            let n = u32::from(a);
            let in_net = |net: [u8; 4], bits: u32| n >> (32 - bits) == u32::from(Ipv4Addr::from(net)) >> (32 - bits);
            !(in_net([0, 0, 0, 0], 8)
                || in_net([10, 0, 0, 0], 8)
                || in_net([100, 64, 0, 0], 10)
                || in_net([127, 0, 0, 0], 8)
                || in_net([169, 254, 0, 0], 16)
                || in_net([172, 16, 0, 0], 12)
                || in_net([192, 0, 0, 0], 24)
                || in_net([192, 0, 2, 0], 24)
                || in_net([192, 88, 99, 0], 24)
                || in_net([192, 168, 0, 0], 16)
                || in_net([198, 18, 0, 0], 15)
                || in_net([198, 51, 100, 0], 24)
                || in_net([203, 0, 113, 0], 24)
                || in_net([224, 0, 0, 0], 4)
                || in_net([240, 0, 0, 0], 4)
                // Cloud services that hold the machine's credentials, and are not in a registry.
                || o == [168, 63, 129, 16])
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return reference_global(IpAddr::V4(v4));
            }
            let n = u128::from(a);
            let in_net = |net: u128, bits: u32| n >> (128 - bits) == net >> (128 - bits);
            let seg = |s: [u16; 8]| s.iter().fold(0u128, |acc, v| acc << 16 | *v as u128);
            // Only 2000::/3 is global unicast.
            in_net(seg([0x2000, 0, 0, 0, 0, 0, 0, 0]), 3)
                && !(in_net(seg([0x2001, 0, 0, 0, 0, 0, 0, 0]), 23)
                    || in_net(seg([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0]), 32)
                    || in_net(seg([0x2002, 0, 0, 0, 0, 0, 0, 0]), 16)
                    || in_net(seg([0x3fff, 0, 0, 0, 0, 0, 0, 0]), 20))
        }
    }
}

pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else { return };
    let text = String::from_utf8_lossy(rest);
    match which % 6 {
        0 => {
            for local in [false, true] {
                if let Ok(p) = Pattern::parse(&text, local) {
                    // What the manifest checker accepts, a policy takes, and shows back unchanged.
                    assert!(p.text.is_ascii(), "{:?}", p.text);
                    assert!(local || !p.is_ip());
                    let again = Pattern::parse(&p.text, local).expect("a pattern's own text parses");
                    assert!(again == p);
                }
            }
        }
        1 => {
            if let Ok((uri, target)) = parse_url(&text) {
                assert!(text.is_ascii());
                assert!(target.host == target.host.to_ascii_lowercase());
                // The target the rules see is what the HTTP library will connect to.
                assert_eq!(uri.host().map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase()), Some(target.host.clone()));
                assert!(uri.authority().is_some_and(|a| !a.as_str().contains('@')));
                // (Port 0 is a URL's, never a pattern's: it can be allowed by nothing.)
                if target.port == 0 {
                    return;
                }
                let policy = Policy { patterns: vec![Pattern::parse(&target.shown(), true).expect("shown is a pattern")], local: true, timeout: Duration::from_secs(1), max_response: 1 };
                assert!(policy.permits(&target), "{target:?}");
                // The std parser agrees on what is an address.
                if let Ok(ip) = target.host.parse::<IpAddr>() {
                    let _ = policy.ip_ok(ip);
                }
            }
        }
        2 => {
            let (base, location) = text.split_once('\n').unwrap_or(("https://a.example/x/y?z=1", &text));
            if let Some(joined) = join(base, location) {
                // A redirect that parses is judged by the same rules as the first request.
                let _ = parse_url(&joined);
            }
        }
        3 => {
            let policy = Policy { patterns: ["https://discord.com", "http://relay.example.org:8080", "https://*.example.com", "http://127.0.0.1:9"].iter().map(|p| Pattern::parse(p, true).unwrap()).collect(), local: true, timeout: Duration::from_secs(5), max_response: 1 << 16 };
            let config: BTreeMap<String, String> = [("hook".to_string(), "1234/abcd".to_string()), ("token".to_string(), "s3cret".to_string())].into();
            if let Ok(request) = pcds_plugin_api::HttpRequest::decode(rest) {
                if let Ok(fetch) = policy.build(&request, &config) {
                    // Whatever went through is a URL on the list, with headers a server can write.
                    let (_, target) = parse_url(&fetch.url).expect("a built request has a URL");
                    assert!(policy.permits(&target));
                    assert!(!fetch.url.contains("${"), "{}", fetch.url);
                    for (k, v) in &fetch.headers {
                        header_ok(k, v).expect("a built header passes the check");
                        assert!(!v.contains('\r') && !v.contains('\n'));
                    }
                    assert!(METHODS.contains(&fetch.method.as_str()));
                    assert!(!format!("{fetch:?}").contains("s3cret"));
                }
            }
        }
        4 => {
            let config: BTreeMap<String, String> = [("a".to_string(), "x${config.a}".to_string())].into();
            let _ = expand(&text, &config);
            let _ = header_ok(text.split('\n').next().unwrap_or(""), text.split('\n').nth(1).unwrap_or(""));
        }
        _ => {
            let bytes: [u8; 16] = rest.iter().copied().chain(std::iter::repeat(0)).take(16).collect::<Vec<_>>().try_into().unwrap();
            let ip: IpAddr = if which & 0x80 != 0 { IpAddr::V6(Ipv6Addr::from(bytes)) } else { IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])) };
            for local in [false, true] {
                let policy = Policy { patterns: vec![], local, timeout: Duration::from_secs(1), max_response: 1 };
                if policy.ip_ok(ip) {
                    assert!(reference_global(ip) || (local && matches!(classify(ip), Class::Local)), "{ip} may be dialled but is not a public address");
                    if !local {
                        assert!(reference_global(ip), "{ip} may be dialled without net.http.local");
                    }
                }
            }
        }
    }
}
