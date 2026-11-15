//! Where carebus-api may send a request on a customer's behalf. Only public internet addresses: never
//! loopback, private, link-local, CGNAT (the tailnet), cluster or LAN names. Used on save for every source URL and
//! LLM provider base_url and again, after DNS resolution, before every outbound call ([`pinned_builder`]: api
//! test-connection, the provider test, the laya gates call, the tenant's qualify client).
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::Serialize;
use serde_json::Value;

/// Why a target was refused. The text is safe to show; it never contains the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub &'static str);

const PRIVATE: &str = "the host is a private, loopback, link-local or CGNAT address";

/// Hostname suffixes that name hosts inside the cluster, the tailnet or a LAN.
const INTERNAL_SUFFIXES: [&str; 7] = [
    ".internal",
    ".svc",
    ".cluster.local",
    ".local",
    ".localhost",
    ".lan",
    ".home.arpa",
];

/// Dotless names (`localhost`, `minio`) and names under an internal suffix. Case-insensitive,
/// trailing dots ignored.
pub fn internal_hostname(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    !h.contains('.')
        || INTERNAL_SUFFIXES
            .iter()
            .any(|s| h.ends_with(s) || h == s[1..])
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback() // 127/8
        || ip.is_private() // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254/16 (cloud metadata)
        || ip.is_broadcast()
        || ip.is_multicast() // 224/4
        || ip.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || a == 0 // 0/8 "this network"
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 CGNAT (the tailnet)
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24 IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // 198.18/15 benchmarking
        || a >= 240) // 240/4 reserved
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4); // ::ffff:a.b.c.d
    }
    let s = ip.segments();
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        // 64:ff9b::/96 NAT64 embeds an IPv4 address
        return public_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
        || s[0] == 0x2002 // 2002::/16 6to4 embeds an IPv4 address that is not checked
        || (s[0] == 0x2001 && s[1] == 0) // 2001::/32 Teredo embeds an IPv4 address that is not checked
        || s[0..6] == [0, 0, 0, 0, 0, 0]) // ::/96 IPv4-compatible (deprecated), incl. ::a.b.c.d
}

/// True only for addresses on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// 127/8 and ::1 only: the test-only loopback allowance never opens any other range.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    }
}

fn allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    is_public(ip) || (allow_loopback && is_loopback(ip))
}

/// Syntax and literal-address rules: http(s), a host, no userinfo, no fragment, not an internal name, not a
/// non-public literal IP (decimal, hex and mapped forms are normalised by the URL parser first).
pub fn check_url(raw: &str, allow_loopback: bool) -> Result<reqwest::Url, Refused> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| Refused("not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refused("only http and https URLs are allowed"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refused(
            "a URL must not carry credentials; use the credentials field",
        ));
    }
    if url.fragment().is_some() {
        return Err(Refused("a URL must not have a #fragment"));
    }
    let host = url.host_str().ok_or(Refused("the URL has no host"))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) if !allowed(ip, allow_loopback) => return Err(Refused(PRIVATE)),
        Ok(_) => {}
        Err(_) if internal_hostname(host) => {
            return Err(Refused(
                "the host is an internal name (*.internal, *.svc, *.local or dotless)",
            ));
        }
        Err(_) => {}
    }
    Ok(url)
}

/// `check_url` + [`pinned_builder`]: the one way an outbound call to a tenant-supplied URL gets its client.
pub async fn check_and_pin(
    raw: &str,
    allow_loopback: bool,
) -> Result<(reqwest::Url, reqwest::ClientBuilder), Refused> {
    let url = check_url(raw, allow_loopback)?;
    let builder = pinned_builder(&url, allow_loopback).await?;
    Ok((url, builder))
}

/// [`pinned_builder`]'s refusal when the host has no address (or the lookup timed out): a network failure, not a
/// policy refusal.
pub const UNRESOLVED: Refused = Refused("the host name does not resolve");

/// A client builder for calls to `url` (already passed `check_url`): no proxy, no redirects, and for a host name
/// one lookup (bounded by [`FETCH_TIMEOUT`]) whose every answer passes `check_addrs`, the connection pinned to the
/// checked address (no second lookup, so DNS rebinding cannot swap it). [`UNRESOLVED`] when the name has no address.
pub async fn pinned_builder(
    url: &reqwest::Url,
    allow_loopback: bool,
) -> Result<reqwest::ClientBuilder, Refused> {
    let host = url
        .host_str()
        .ok_or(Refused("the URL has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if host.parse::<IpAddr>().is_err() {
        let addrs: Vec<SocketAddr> = match tokio::time::timeout(
            FETCH_TIMEOUT,
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        {
            Ok(Ok(a)) => a.collect(),
            _ => return Err(UNRESOLVED),
        };
        if addrs.is_empty() {
            return Err(UNRESOLVED);
        }
        let pinned = check_addrs(&addrs, allow_loopback)?;
        builder = builder.resolve(&host, pinned);
    }
    Ok(builder)
}

/// After DNS: every answer must be allowed (one private answer refuses the host). Returns the address to pin.
pub fn check_addrs(addrs: &[SocketAddr], allow_loopback: bool) -> Result<SocketAddr, Refused> {
    let first = *addrs
        .first()
        .ok_or(Refused("the host name does not resolve"))?;
    if addrs.iter().any(|a| !allowed(a.ip(), allow_loopback)) {
        return Err(Refused(
            "the host name resolves to a private, loopback, link-local or CGNAT address",
        ));
    }
    Ok(first)
}

pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub const MAX_SAMPLE_BYTES: usize = 256 * 1024;
const EXCERPT_CHARS: usize = 2000;
const REDACTED: &str = "[redacted]";

/// How the stored credential is presented.
pub enum Auth<'a> {
    None,
    Bearer(&'a str),
    Header(&'a str, &'a str),
}

#[derive(Debug, Serialize)]
pub struct ApiSample {
    pub ok: bool,
    pub status: Option<u16>,
    pub detail: String,
    pub sample: Value,
    pub fields: Vec<String>,
    pub truncated: bool,
}

impl ApiSample {
    fn failed(detail: &str) -> Self {
        Self {
            ok: false,
            status: None,
            detail: detail.into(),
            sample: Value::Null,
            fields: Vec::new(),
            truncated: false,
        }
    }
}

/// The first item of a JSON array, a JSON object as is, other JSON as is, else the first 2000 characters as text.
/// `fields` are the sample's top-level keys, sorted.
pub fn sample_of(body: &[u8]) -> (Value, Vec<String>) {
    let sample = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Array(a)) => a.into_iter().next().unwrap_or(Value::Null),
        Ok(v) => v,
        Err(_) => Value::String(
            String::from_utf8_lossy(body)
                .chars()
                .take(EXCERPT_CHARS)
                .collect(),
        ),
    };
    // sorted: the order is the same whether or not serde_json keeps insertion order
    let mut fields: Vec<String> = sample
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    fields.sort();
    (sample, fields)
}

/// ONE GET to `raw` with the credential.
/// - The host is checked by `check_url`, resolved once, every answer checked by `check_addrs`, and the
///   connection pinned to the checked address (no second lookup, so DNS rebinding cannot swap it).
/// - No proxy, no redirects, 5 s timeout, body capped at 256 KiB.
/// - `Err` only for a refused target. Network failures are `Ok` with `ok: false` and never echo the URL.
pub async fn fetch_sample(
    raw: &str,
    auth: Auth<'_>,
    allow_loopback: bool,
) -> Result<ApiSample, Refused> {
    // ONE 5 s deadline for DNS + connect + request + body, not 5 s per phase.
    match tokio::time::timeout(FETCH_TIMEOUT, fetch_inner(raw, auth, allow_loopback)).await {
        Ok(r) => r,
        Err(_) => Ok(ApiSample::failed("timed out after 5 s")),
    }
}

/// Replaces every occurrence of `secret` with `[redacted]`: in text, in JSON string values and in object keys.
pub fn redact(v: Value, secret: &str) -> Value {
    if secret.is_empty() {
        return v;
    }
    match v {
        Value::String(s) => Value::String(s.replace(secret, REDACTED)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| redact(x, secret)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k.replace(secret, REDACTED), redact(x, secret)))
                .collect(),
        ),
        other => other,
    }
}

async fn fetch_inner(
    raw: &str,
    auth: Auth<'_>,
    allow_loopback: bool,
) -> Result<ApiSample, Refused> {
    let secret: String = match &auth {
        Auth::None => String::new(),
        Auth::Bearer(t) | Auth::Header(_, t) => (*t).to_string(),
    };
    let (url, builder) = match check_and_pin(raw, allow_loopback).await {
        Ok(p) => p,
        Err(UNRESOLVED) => return Ok(ApiSample::failed(UNRESOLVED.0)),
        Err(r) => return Err(r),
    };
    let client = builder
        .build()
        .map_err(|_| Refused("the HTTP client could not be built"))?;
    let mut req = client.get(url);
    match auth {
        Auth::None => {}
        Auth::Bearer(t) => req = req.bearer_auth(t),
        Auth::Header(name, value) => req = req.header(name, value),
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let detail = if e.is_timeout() {
                "timed out after 5 s"
            } else if e.is_builder() {
                "the request could not be built (check the header name)"
            } else {
                "unreachable"
            };
            return Ok(ApiSample::failed(detail));
        }
    };
    let status = resp.status();
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                let room = MAX_SAMPLE_BYTES - body.len();
                if c.len() > room {
                    body.extend_from_slice(&c[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true; // timed out or broke mid-body: show what arrived
                break;
            }
        }
    }
    // Scrub before sampling (a secret cut by the excerpt limit would survive as a prefix), then again on the
    // parsed value (a secret that was JSON-escaped in the body).
    if !secret.is_empty() {
        let text = String::from_utf8_lossy(&body).replace(&secret, REDACTED);
        body = text.into_bytes();
    }
    let (sample, _) = sample_of(&body);
    let sample = redact(sample, &secret);
    let mut fields: Vec<String> = sample
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    fields.sort();
    let detail = if status.is_success() {
        "reachable"
    } else if status.is_redirection() {
        "answered with a redirect, which is not followed"
    } else {
        "answered with an error status"
    };
    Ok(ApiSample {
        ok: status.is_success(),
        status: Some(status.as_u16()),
        detail: detail.into(),
        sample,
        fields,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_reserved_ranges_are_not_public() {
        for ip in [
            "127.0.0.1",
            "127.255.0.9",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.9.9",
            "192.168.1.119",
            "169.254.169.254",
            "100.64.0.1",
            "100.64.0.7",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.0.8",
            "198.18.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
            "::127.0.0.1",
            "2002:7f00:1::1",                       // 6to4 embedding 127.0.0.1
            "2002:0a00:0001::",                     // 6to4 embedding 10.0.0.1
            "2002:808:808::1", // 6to4 even embedding a public v4: not routable here
            "2001::1",         // Teredo
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} must be refused");
        }
    }

    #[test]
    fn public_addresses_pass() {
        for ip in [
            "1.1.1.1",
            "217.160.28.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} is public");
        }
    }

    #[test]
    fn internal_names() {
        for h in [
            "localhost",
            "minio",
            "postgres.internal",
            "carebus-api.some-ns.svc",
            "x.svc.cluster.local",
            "printer.local",
            "API.INTERNAL.",
            "internal",
        ] {
            assert!(internal_hostname(h), "{h}");
        }
        for h in ["api.example.com", "internal.example.com", "svc.example.org"] {
            assert!(!internal_hostname(h), "{h}");
        }
    }

    #[test]
    fn check_url_refuses_internal_and_private_targets() {
        for u in [
            "http://127.0.0.1:9/x",
            "http://10.0.0.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://api.internal/x",
            "http://carebus-api.some-ns.svc/x",
            "http://x.svc.cluster.local/",
            "http://localhost/x",
            "http://minio:9000/",
            "ftp://example.com/x",
            "http://user:pw@example.com/",
            "https://example.com/#frag",
            "not a url",
        ] {
            assert!(check_url(u, false).is_err(), "{u} must be refused");
        }
        assert!(check_url("https://api.example.com/v2/customers?page=1", false).is_ok());
        assert!(
            check_url("http://127.0.0.1:9/x", true).is_ok(),
            "the test-only loopback allowance"
        );
        assert!(
            check_url("http://10.0.0.1/x", true).is_err(),
            "...opens nothing else"
        );
        assert!(check_url("http://localhost/x", true).is_err());
    }

    #[test]
    fn every_resolved_address_must_be_public() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            check_addrs(&[a("93.184.216.34:443")], false),
            Ok(a("93.184.216.34:443"))
        );
        assert!(
            check_addrs(&[a("93.184.216.34:443"), a("10.0.0.5:443")], false).is_err(),
            "one private answer refuses the host"
        );
        assert!(check_addrs(&[a("[::1]:443")], false).is_err());
        assert!(check_addrs(&[], false).is_err());
        assert!(check_addrs(&[a("127.0.0.1:80")], true).is_ok());
        assert!(check_addrs(&[a("192.168.1.1:80")], true).is_err());
    }

    #[test]
    fn sample_is_the_first_item_the_object_or_a_text_excerpt() {
        let (s, f) = sample_of(br#"[{"b":1,"a":2},{"c":3}]"#);
        assert_eq!(s, serde_json::json!({"a": 2, "b": 1}));
        assert_eq!(f, ["a", "b"]);
        let (s, f) = sample_of(br#"{"items":[1],"next":null}"#);
        assert_eq!(f, ["items", "next"]);
        assert!(s.is_object());
        assert_eq!(sample_of(b"[]").0, serde_json::Value::Null);
        let (s, f) = sample_of(b"plain text body");
        assert_eq!((s, f.len()), (serde_json::json!("plain text body"), 0));
        let long = "x".repeat(5000);
        assert_eq!(sample_of(long.as_bytes()).0.as_str().unwrap().len(), 2000);
    }
}
