//! HTTP clients that use the Windows proxy setting (设置 → 网络 → 代理),
//! which reqwest does not read by itself.  Many users reach GitHub only
//! through such a proxy.

use reqwest::blocking::{Client, ClientBuilder};
use std::time::Duration;

const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

#[cfg(not(windows))]
fn system_proxy() -> Option<(String, String)> {
    None
}

/// The manual proxy from Internet Settings, as `(proxy URL, bypass list)`.
#[cfg(windows)]
fn system_proxy() -> Option<(String, String)> {
    if crate::links::registry::read_dword(INTERNET_SETTINGS, "ProxyEnable")? == 0 {
        return None;
    }
    let server = crate::links::registry::read(INTERNET_SETTINGS, Some("ProxyServer"))?;
    let bypass =
        crate::links::registry::read(INTERNET_SETTINGS, Some("ProxyOverride")).unwrap_or_default();
    Some((parse_proxy_server(&server)?, bypass))
}

/// `host:port`, or `http=host:port;https=host:port;…` (https wins).
pub fn parse_proxy_server(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    let chosen = if server.contains('=') {
        let entries: Vec<(&str, &str)> = server
            .split(';')
            .filter_map(|part| part.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        ["https", "http"].iter().find_map(|scheme| {
            entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(scheme))
                .map(|(_, v)| *v)
        })?
    } else {
        server
    };
    if chosen.is_empty() {
        return None;
    }
    Some(if chosen.contains("://") {
        chosen.to_owned()
    } else {
        format!("http://{chosen}")
    })
}

/// Whether `host` matches the `ProxyOverride` list (`*.corp;<local>`).
pub fn bypassed(host: &str, list: &str) -> bool {
    let host = host.to_ascii_lowercase();
    list.split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .any(|entry| {
            if entry == "<local>" {
                return !host.contains('.') || host == "127.0.0.1" || host == "localhost";
            }
            match entry.strip_prefix("*.") {
                Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
                None => match entry.strip_suffix('*') {
                    Some(prefix) => host.starts_with(prefix),
                    None => host == entry,
                },
            }
        })
}

/// A client builder with Marina's user agent, timeouts and the system proxy.
pub fn builder(timeout: Duration) -> ClientBuilder {
    let builder = Client::builder()
        .user_agent(concat!("DSH-Marina/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout);
    let has_env_proxy = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| std::env::var(name).is_ok_and(|v| !v.is_empty()));
    if has_env_proxy {
        return builder;
    }
    // A custom proxy bypasses reqwest's own handling of `NO_PROXY`, so fold
    // that list into our bypass check.
    let no_proxy = ["NO_PROXY", "no_proxy"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_default();
    match system_proxy().and_then(|(url, bypass)| Some((reqwest::Url::parse(&url).ok()?, bypass))) {
        Some((url, bypass)) => builder.proxy(reqwest::Proxy::custom(move |target| {
            let host = target.host_str().unwrap_or_default();
            (!bypassed(host, &bypass) && !bypassed(host, &no_proxy)).then(|| url.clone())
        })),
        None => builder,
    }
}

pub fn client(timeout: Duration) -> reqwest::Result<Client> {
    builder(timeout).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_server_forms() {
        assert_eq!(
            parse_proxy_server("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            parse_proxy_server("http=a:1;https=b:2;ftp=c:3").as_deref(),
            Some("http://b:2")
        );
        assert_eq!(
            parse_proxy_server("http=a:1").as_deref(),
            Some("http://a:1")
        );
        assert_eq!(
            parse_proxy_server("socks5://h:1").as_deref(),
            Some("socks5://h:1")
        );
        assert_eq!(parse_proxy_server(" "), None);
    }

    #[test]
    fn matches_bypass_entries() {
        let list = "localhost;127.*;*.corp.example;<local>";
        assert!(bypassed("localhost", list));
        assert!(bypassed("127.0.0.1", list));
        assert!(bypassed("git.corp.example", list));
        assert!(bypassed("intranet", list));
        assert!(!bypassed("api.github.com", list));
    }
}
