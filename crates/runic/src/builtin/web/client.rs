use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use futures::StreamExt;
use url::Host;

const USER_AGENT: &str = "runic/0.1 (web_fetch)";
const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_REDIRECTS: usize = 5;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Fetched {
    pub body: String,
    pub url: reqwest::Url,
    pub content_type: String,
}

#[derive(Clone)]
pub struct WebClient {
    inner: reqwest::Client,
    max_body_bytes: usize,
    max_redirects: usize,
}

impl Default for WebClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebClient {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        let inner = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT)
            // Redirects are followed by hand so every hop is re-guarded.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client builds with static config");
        Self {
            inner,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }

    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes.max(1);
        self
    }

    pub fn max_redirects(mut self, hops: usize) -> Self {
        self.max_redirects = hops;
        self
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.inner
    }

    pub async fn guard(&self, url: &str) -> Result<reqwest::Url, String> {
        guard_url(url).await
    }

    pub async fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let mut current = url.to_string();
        for _ in 0..=self.max_redirects {
            let target = self.guard(&current).await?;
            let resp = self
                .inner
                .get(target.clone())
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status();

            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or("redirect without a Location header")?;
                current = target
                    .join(location)
                    .map_err(|e| format!("bad redirect target: {e}"))?
                    .to_string();
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }

            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_textual = content_type.is_empty()
                || content_type.contains("html")
                || content_type.contains("text/")
                || content_type.contains("json")
                || content_type.contains("xml");
            if !is_textual {
                return Err(format!("unsupported content-type '{content_type}'"));
            }

            let bytes = self.read_capped(resp).await?;
            return Ok(Fetched {
                body: String::from_utf8_lossy(&bytes).into_owned(),
                url: target,
                content_type,
            });
        }
        Err("too many redirects".into())
    }

    async fn read_capped(&self, resp: reqwest::Response) -> Result<Vec<u8>, String> {
        let cap = self.max_body_bytes;
        if resp.content_length().is_some_and(|len| len > cap as u64) {
            return Err(format!("response exceeds {cap} bytes"));
        }
        let mut buffered = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("read failed: {e}"))?;
            if buffered.len().saturating_add(chunk.len()) > cap {
                return Err(format!("response exceeds {cap} bytes"));
            }
            buffered.extend_from_slice(&chunk);
        }
        Ok(buffered)
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || octets[0] == 0
        || octets[0] >= 224
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0])
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || embedded_ipv4(ip).is_some_and(is_blocked_v4)
}

fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    if segments[..5] != [0, 0, 0, 0, 0] {
        return None;
    }
    match segments[5] {
        0xffff | 0 => Some(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        )),
        _ => None,
    }
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_v4(ip),
        IpAddr::V6(ip) => is_blocked_v6(ip),
    }
}

fn is_local_name(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
}

pub async fn guard_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme '{other}' (only http/https)")),
    }

    match parsed.host() {
        Some(Host::Ipv4(ip)) if is_blocked_v4(ip) => {
            Err("refusing to fetch a private/reserved IP".into())
        }
        Some(Host::Ipv6(ip)) if is_blocked_v6(ip) => {
            Err("refusing to fetch a private/reserved IP".into())
        }
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => Ok(parsed),
        Some(Host::Domain(host)) => {
            if is_local_name(host) {
                return Err("refusing to fetch a local/loopback host".into());
            }
            let port = parsed
                .port_or_known_default()
                .ok_or("url has no known port")?;
            let addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| format!("dns lookup failed: {e}"))?;
            let mut resolved = false;
            for addr in addrs {
                resolved = true;
                if is_blocked_ip(addr.ip()) {
                    return Err("host resolves to a private/reserved IP".into());
                }
            }
            if !resolved {
                return Err("host did not resolve".into());
            }
            Ok(parsed)
        }
        None => Err("url has no host".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_reserved() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.1.2.3",
            "::1",
            "fc00::1",
        ] {
            assert!(is_blocked_ip(blocked.parse().unwrap()), "{blocked}");
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!is_blocked_ip(allowed.parse().unwrap()), "{allowed}");
        }
    }

    #[test]
    fn embedded_ipv4_covers_mapped_and_compatible_forms() {
        assert!(is_blocked_ip("::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip("::169.254.169.254".parse().unwrap()));
        assert!(!is_blocked_ip("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_local_names_and_bad_schemes() {
        assert!(guard_url("http://localhost/x").await.is_err());
        assert!(guard_url("http://foo.local/").await.is_err());
        assert!(guard_url("http://127.0.0.1:8080/").await.is_err());
        assert!(
            guard_url("http://169.254.169.254/latest/meta-data")
                .await
                .is_err()
        );
        assert!(guard_url("ftp://example.com/").await.is_err());
        assert!(guard_url("not a url").await.is_err());
    }

    #[tokio::test]
    async fn ipv6_literals_are_refused_by_the_guard_not_by_the_resolver() {
        for target in [
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::127.0.0.1]/",
            "http://[64:ff9b::7f00:1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[2001:db8::1]/",
        ] {
            let error = guard_url(target).await.expect_err(target);
            assert!(
                error.contains("private/reserved"),
                "{target} was refused for the wrong reason: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_public_ipv6_literal_is_allowed() {
        assert!(guard_url("http://[2606:4700:4700::1111]/").await.is_ok());
    }

    #[tokio::test]
    async fn the_zero_network_is_refused() {
        assert!(guard_url("http://0.1.2.3/").await.is_err());
    }
}
