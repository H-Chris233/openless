//! Shared validation for user-configurable HTTP endpoints.
//!
//! Provider validation and the real request path must call the same function;
//! otherwise a saved endpoint can bypass the checks performed by the
//! "validate connection" button.
//!
//! Only URL well-formedness is enforced: the value must be a valid URL with a
//! host and an `http`/`https` scheme. Address reachability is deliberately not
//! restricted — endpoints are explicitly configured by the user (LAN gateways,
//! internal DNS names, hosts-file aliases, public hosts, etc.), and the
//! settings UI shows an in-app warning when an `http://` endpoint is entered.
//! The user decides.

use std::net::IpAddr;

pub(crate) struct ResolvedEndpoint {
    pub(crate) host: String,
    pub(crate) addrs: Vec<std::net::SocketAddr>,
}

/// Validate a user-configured endpoint. Format-only: must be a valid `http(s)`
/// URL with a host. No SSRF-style address restrictions are applied.
pub(crate) fn validate_http_endpoint(raw: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(raw).map_err(|e| anyhow::anyhow!("endpoint 不是合法 URL：{e}"))?;
    url.host_str()
        .ok_or_else(|| anyhow::anyhow!("endpoint 缺少主机名"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("endpoint 必须使用 http 或 https：{raw}");
    }
    Ok(())
}

/// Resolve a hostname once, and return the addresses so the HTTP client can pin
/// this exact resolution and avoid DNS rebinding. No address restrictions are
/// applied to the resolved results.
pub(crate) async fn resolve_http_endpoint(raw: &str) -> anyhow::Result<Option<ResolvedEndpoint>> {
    validate_http_endpoint(raw)?;
    let url = url::Url::parse(raw)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("endpoint 缺少主机名"))?;
    if host.parse::<IpAddr>().is_ok() {
        return Ok(None);
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("endpoint 缺少端口"))?;
    let addrs: Vec<_> = tokio::net::lookup_host((host, port)).await?.collect();
    if addrs.is_empty() {
        anyhow::bail!("endpoint 主机名无法解析：{host}");
    }
    Ok(Some(ResolvedEndpoint {
        host: host.to_string(),
        addrs,
    }))
}

#[cfg(test)]
mod tests {
    use super::validate_http_endpoint;

    #[test]
    fn accepts_http_anywhere_user_chooses() {
        // 地址选择权完全交给用户：公网域名、局域网、公网 IP、本地、元数据地址一律
        // 放行，前端对 http:// 输入展示明文风险提示（user decides）。
        validate_http_endpoint("http://example.com:12345/")
            .expect("public HTTP hostname must be allowed");
        validate_http_endpoint("http://api.example.com/v1/audio/transcriptions")
            .expect("public HTTP hostname must be allowed");
        validate_http_endpoint("http://1.2.3.4/v1").expect("public literal IP HTTP must be allowed");
        validate_http_endpoint("http://192.168.1.50:9000/v1")
            .expect("LAN HTTP endpoint must be allowed");
        validate_http_endpoint("http://localhost:9000/v1")
            .expect("localhost HTTP endpoint must be allowed");
        validate_http_endpoint("http://169.254.169.254/v1")
            .expect("metadata address must be allowed (user decides)");
        validate_http_endpoint("http://100.64.0.1/v1")
            .expect("CGNAT address must be allowed (user decides)");
        validate_http_endpoint("http://metadata.google.internal/v1")
            .expect("metadata hostname must be allowed (user decides)");
        validate_http_endpoint("https://example.com:12345/")
            .expect("HTTPS hostname must be allowed");
    }

    #[test]
    fn rejects_malformed_or_non_http_urls() {
        assert!(validate_http_endpoint("not a url").is_err());
        assert!(validate_http_endpoint("ftp://example.com/").is_err());
        assert!(validate_http_endpoint("wss://example.com/").is_err());
    }
}
