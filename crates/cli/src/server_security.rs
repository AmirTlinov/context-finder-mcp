use anyhow::{Context as AnyhowContext, Result};
use std::net::SocketAddr;

pub(crate) const AUTH_TOKEN_ENV: &str = "CONTEXT_AUTH_TOKEN";

#[derive(Clone, Debug)]
pub(crate) struct AuthToken {
    token: String,
}

impl AuthToken {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Option<Self>> {
        let Some(raw) = raw else {
            return Ok(None);
        };

        let token = raw.trim();
        if token.is_empty() {
            anyhow::bail!("auth token must be non-empty")
        }

        Ok(Some(Self {
            token: token.to_string(),
        }))
    }

    pub(crate) fn matches_http_authorization_header(&self, header_value: &str) -> bool {
        // Accept only RFC6750-ish "Bearer <token>".
        let header_value = header_value.trim();
        let Some(rest) = header_value.strip_prefix("Bearer ") else {
            return false;
        };
        constant_time_eq(rest.trim(), &self.token)
    }
}

pub(crate) async fn resolve_guarded_bind_addrs(
    bind: &str,
    public: bool,
) -> Result<Vec<SocketAddr>> {
    let addrs = resolve_bind_addrs(bind).await?;
    enforce_bind_guard_for_addrs(bind, &addrs, public)?;
    Ok(addrs)
}

pub(crate) fn choose_preferred_bind_addr(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    addrs
        .iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| addrs.first().copied())
}

async fn resolve_bind_addrs(bind: &str) -> Result<Vec<SocketAddr>> {
    // Prefer resolving via Tokio so "localhost" behaves as expected.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(bind)
        .await
        .with_context(|| format!("Failed to resolve bind address: {bind}"))?
        .collect();

    if addrs.is_empty() {
        anyhow::bail!("Bind address resolved to zero socket addrs: {bind}")
    }
    Ok(addrs)
}

fn enforce_bind_guard_for_addrs(bind: &str, addrs: &[SocketAddr], public: bool) -> Result<()> {
    let any_non_loopback = addrs.iter().any(|addr| !addr.ip().is_loopback());
    if any_non_loopback && !public {
        anyhow::bail!(
            "Refusing to bind to non-loopback address without --public: {bind}. If you want to expose the server, pass --public and set CONTEXT_AUTH_TOKEN (or --auth-token)."
        )
    }
    Ok(())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut diff: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_token_parses_and_matches_bearer_header() {
        let token = AuthToken::parse(Some("  secret  ")).unwrap().unwrap();
        assert!(token.matches_http_authorization_header("Bearer secret"));
        assert!(token.matches_http_authorization_header("Bearer  secret  "));
        assert!(!token.matches_http_authorization_header("secret"));
        assert!(!token.matches_http_authorization_header("Bearer wrong"));
    }

    #[tokio::test]
    async fn bind_guard_requires_public_for_non_loopback() {
        resolve_guarded_bind_addrs("127.0.0.1:0", false)
            .await
            .unwrap();

        assert!(resolve_guarded_bind_addrs("0.0.0.0:0", false)
            .await
            .is_err());
        resolve_guarded_bind_addrs("0.0.0.0:0", true).await.unwrap();
    }
}
