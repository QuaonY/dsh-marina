//! Startup success signal for a DSH desktop instance (design §FR-5, V13).
//!
//! DSH prints `dsh web: http://127.0.0.1:<port>/?token=<t>` once its local
//! backend is listening.  Marina keeps only the port (never the token) and
//! treats "the port accepts a TCP connection" as healthy, which is the V13
//! fallback criterion because the HTTP API requires the token.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WEB_MARKER: &str = "dsh web:";

/// Extract the loopback port from a `dsh web:` stdout line.
pub fn parse_web_port(line: &str) -> Option<u16> {
    let rest = &line[line.find(WEB_MARKER)? + WEB_MARKER.len()..];
    let rest = rest.trim_start();
    let authority = rest
        .strip_prefix("http://")
        .or_else(|| rest.strip_prefix("https://"))?;
    let authority = authority
        .split(['/', '?', '#', ' '])
        .next()
        .unwrap_or_default();
    let (host, port) = authority.rsplit_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

/// Whether something accepts connections on `127.0.0.1:<port>` (or `[::1]`,
/// which DSH announces on IPv6-only machines).
pub fn probe_port(port: u16, timeout: Duration) -> bool {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&v4, timeout).is_ok() {
        return true;
    }
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&v6, timeout).is_ok()
}

/// Port announced by a running instance, shared between the stdout reader
/// thread and the UI monitor.
#[derive(Debug, Clone, Default)]
pub struct WebEndpoint(Arc<Mutex<Option<u16>>>);

impl WebEndpoint {
    pub fn set(&self, port: u16) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(port);
        }
    }

    pub fn port(&self) -> Option<u16> {
        self.0.lock().ok().and_then(|slot| *slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_the_port_and_ignores_the_token() {
        assert_eq!(
            parse_web_port("dsh web: http://127.0.0.1:61105/?token=secret"),
            Some(61105)
        );
        assert_eq!(
            parse_web_port("[host] dsh web: http://localhost:8080/"),
            Some(8080)
        );
        assert_eq!(parse_web_port("dsh web: http://127.0.0.1:0/"), None);
        assert_eq!(parse_web_port("dsh web: http://10.0.0.1:80/"), None);
        assert_eq!(parse_web_port("listening on 127.0.0.1:80"), None);
    }

    #[test]
    fn probe_detects_a_listening_loopback_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_port(port, Duration::from_millis(500)));
        drop(listener);
    }

    #[test]
    fn endpoint_is_shared_between_clones() {
        let endpoint = WebEndpoint::default();
        let reader = endpoint.clone();
        assert_eq!(reader.port(), None);
        endpoint.set(1234);
        assert_eq!(reader.port(), Some(1234));
    }
}
