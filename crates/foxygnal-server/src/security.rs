//! Request guards: Host and Origin checks for every request, plus an access
//! token when listening beyond loopback.
//!
//! - **Host** must be a loopback name unless `--allow-remote` is set. This
//!   defeats DNS rebinding: a hostile page can point its own domain at
//!   127.0.0.1, but the browser still sends that domain as Host.
//! - **Origin**, when present, must match Host. Browsers send it on
//!   cross-site fetches and on every WebSocket upgrade, so another site open
//!   in the same browser cannot drive the engine.
//! - **Token** (only with `--allow-remote`): `?token=` on any URL sets a
//!   cookie and redirects; afterwards the cookie or `Authorization: Bearer`
//!   is accepted.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const COOKIE: &str = "foxygnal_token";

pub struct Security {
    pub allow_remote: bool,
    pub token: Option<String>,
}

impl Security {
    /// A fresh random access token (128 bits, hex).
    pub fn random_token() -> String {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("no OS random source");
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}

fn reject(status: StatusCode, msg: &'static str) -> Response {
    (status, msg).into_response()
}

/// Host header without the port, brackets kept for IPv6.
fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        return host
            .split_once(']')
            .map_or(host, |(h, _)| &host[..=h.len()]);
    }
    host.rsplit_once(':').map_or(host, |(h, _)| h)
}

fn is_loopback_host(host: &str) -> bool {
    let h = host_name(host).to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || h == "[::1]" || h.starts_with("127.")
}

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v.strip_prefix("Bearer ") {
            return Some(t.trim());
        }
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
}

/// Splits `token=` out of the query. Returns (token, rest of the query).
fn query_token(uri: &Uri) -> Option<(String, String)> {
    let q = uri.query()?;
    let mut token = None;
    let rest: Vec<&str> = q
        .split('&')
        .filter(|kv| match kv.strip_prefix("token=") {
            Some(t) => {
                token = Some(t.to_owned());
                false
            }
            None => true,
        })
        .collect();
    Some((token?, rest.join("&")))
}

pub async fn guard(State(sec): State<Arc<Security>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return reject(StatusCode::BAD_REQUEST, "missing Host header");
    };
    if !sec.allow_remote && !is_loopback_host(host) {
        return reject(
            StatusCode::MISDIRECTED_REQUEST,
            "foxygnal only answers to localhost; start it with --allow-remote to change that",
        );
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let ok = origin.to_str().is_ok_and(|o| {
            o.strip_prefix("http://")
                .or_else(|| o.strip_prefix("https://"))
                .is_some_and(|o| o.eq_ignore_ascii_case(host))
        });
        if !ok {
            return reject(StatusCode::FORBIDDEN, "cross-origin request refused");
        }
    }

    let Some(expected) = &sec.token else {
        return next.run(req).await;
    };
    if let Some((t, rest)) = query_token(req.uri()) {
        if !eq_ct(t.as_bytes(), expected.as_bytes()) {
            return reject(StatusCode::UNAUTHORIZED, "wrong access token");
        }
        let path = req.uri().path();
        let location = if rest.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{rest}")
        };
        let cookie = format!("{COOKIE}={t}; Path=/; HttpOnly; SameSite=Strict");
        let mut resp = StatusCode::SEE_OTHER.into_response();
        let h = resp.headers_mut();
        if let (Ok(loc), Ok(c)) = (
            HeaderValue::from_str(&location),
            HeaderValue::from_str(&cookie),
        ) {
            h.insert(header::LOCATION, loc);
            h.insert(header::SET_COOKIE, c);
        }
        return resp;
    }
    match presented_token(headers) {
        Some(t) if eq_ct(t.as_bytes(), expected.as_bytes()) => next.run(req).await,
        _ => reject(
            StatusCode::UNAUTHORIZED,
            "access token required: open the URL that `foxygnal serve` printed",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "localhost:7355",
            "127.0.0.1:5173",
            "[::1]:7355",
            "[::1]",
            "foo.localhost:1",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "evil.example:7355",
            "localhost.evil.example",
            "192.168.1.2:7355",
            "[::2]:7355",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn query_token_is_split_out() {
        let uri: Uri = "/x?a=1&token=abc&b=2".parse().unwrap();
        assert_eq!(query_token(&uri), Some(("abc".into(), "a=1&b=2".into())));
        let uri: Uri = "/x?a=1".parse().unwrap();
        assert_eq!(query_token(&uri), None);
    }
}
