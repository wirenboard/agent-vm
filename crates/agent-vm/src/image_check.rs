//! Cheap "is the registry tag newer than our cache?" probe.
//!
//! We deliberately keep pulls off the launch hot-path (a fresh-cache pull
//! takes minutes) by leaving `PullPolicy::IfMissing` on every sandbox
//! create. To still let the user know when there's an update worth
//! fetching, we do a single HTTP HEAD against the registry's manifests/
//! endpoint before launch and compare the `Docker-Content-Digest` header
//! against what microsandbox has cached locally.
//!
//! Failures (registry unreachable, weird response, unparseable reference)
//! are not propagated — the launch should proceed in offline scenarios.

use std::time::Duration;

use anyhow::Result;

/// What we learned about the image relative to the registry.
pub enum UpdateState {
    /// No cached digest locally — first run, microsandbox will pull on
    /// create anyway.
    NotCached,
    /// Cached digest matches the registry. Nothing to say.
    UpToDate,
    /// Registry has a different manifest — print the banner.
    UpdateAvailable {
        /// Short prefix of the cached digest.
        cached: String,
        /// Short prefix of the registry digest.
        remote: String,
    },
}

/// Compare the marker for the last successful pull against the registry.
/// Returns `Ok(None)` when we can't decide (registry unreachable,
/// malformed reference, etc.).
pub async fn check_for_update(image_ref: &str) -> Result<Option<UpdateState>> {
    let cached = crate::pulled_marker::read(image_ref);
    let Some(remote) = fetch_remote_digest(image_ref).await else {
        return Ok(None);
    };
    let state = match cached {
        None => UpdateState::NotCached,
        Some(c) if c == remote => UpdateState::UpToDate,
        Some(c) => UpdateState::UpdateAvailable {
            cached: short(&c),
            remote: short(&remote),
        },
    };
    Ok(Some(state))
}

/// Best-effort fetch of the per-platform manifest digest. Returns None
/// on any failure so callers can decide what to do with the silence.
pub async fn fetch_remote_digest(image_ref: &str) -> Option<String> {
    let parsed = ParsedRef::parse(image_ref)?;
    match remote_manifest_digest(&parsed).await {
        Ok(Some(d)) => {
            tracing::debug!(image = %image_ref, digest = %d, "registry update probe");
            Some(d)
        }
        Ok(None) => {
            // Reachable registry, but we couldn't pin a comparable
            // digest (no matching platform entry, private image we
            // can't auth to, etc.). Stay quiet at launch.
            tracing::debug!(image = %image_ref, "registry update probe: no comparable digest");
            None
        }
        Err(e) => {
            // Offline / DNS / TLS — expected sometimes; never fatal.
            tracing::debug!(image = %image_ref, error = %e, "registry update probe failed");
            None
        }
    }
}

async fn remote_manifest_digest(parsed: &ParsedRef) -> Result<Option<String>> {
    // Microsandbox stores the *per-platform* manifest digest, not the
    // multi-arch index digest. If we naively HEAD the tag we get the
    // index digest, which churns every push even when the underlying
    // platform manifest is identical (e.g. a `docker tag` + push that
    // reuses all layers). So:
    //
    // 1. GET the index, find the linux/amd64 entry.
    // 2. Return that entry's digest.
    //
    // If the tag points directly at a single-arch manifest (no index),
    // step 1 fails to parse as an index and we fall back to the digest
    // header from the GET response itself.
    let scheme = if parsed.is_insecure { "http" } else { "https" };
    let url = format!(
        "{scheme}://{host}/v2/{name}/manifests/{tag}",
        host = parsed.host,
        name = parsed.name,
        tag = parsed.tag,
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = get_manifest_with_auth(&client, &url).await?;
    let Some(resp) = resp else {
        return Ok(None);
    };
    let direct_digest = resp
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Ok(direct_digest),
    };
    // Multi-arch index: walk manifests[] looking for linux/amd64.
    if let Some(arr) = body.get("manifests").and_then(|v| v.as_array()) {
        for entry in arr {
            let os = entry
                .pointer("/platform/os")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arch = entry
                .pointer("/platform/architecture")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // x86_64-host pin; the rewrite is x86_64-only in v1.
            if os == "linux" && arch == "amd64"
                && let Some(d) = entry.get("digest").and_then(|v| v.as_str())
            {
                return Ok(Some(d.to_string()));
            }
        }
        // No matching platform entry → can't compare.
        return Ok(None);
    }
    // Single-arch manifest: the digest header is the right one.
    Ok(direct_digest)
}

/// The Accept set every manifest GET sends — both the v2 single-arch
/// manifest media types and the multi-arch index types, so the registry
/// hands back the index when one exists.
const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.oci.image.index.v1+json",
);

/// GET a manifest, performing the registry token handshake when the
/// registry demands one.
///
/// Token-auth registries (ghcr.io, Docker Hub, …) answer an anonymous
/// manifest GET with `401` and a `WWW-Authenticate: Bearer realm=...,
/// service=...,scope=...` challenge — even for *public* images. The
/// caller must then fetch a bearer token from `realm` and retry. The
/// previous implementation skipped this entirely and treated the `401`
/// as "registry unreachable", so the update-available banner never
/// fired for the default `ghcr.io/...` image.
///
/// Returns `Ok(None)` on any non-success (offline, private image we
/// can't auth to, malformed challenge) so the launch path stays quiet
/// rather than failing.
async fn get_manifest_with_auth(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<reqwest::Response>> {
    let first = client
        .get(url)
        .header("Accept", MANIFEST_ACCEPT)
        .send()
        .await?;
    tracing::debug!(%url, status = %first.status(), "manifest GET (unauthenticated)");
    if first.status().is_success() {
        return Ok(Some(first));
    }
    if first.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    // Parse the Bearer challenge and fetch an (anonymous) token.
    let Some(challenge) = first
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .and_then(BearerChallenge::parse)
    else {
        return Ok(None);
    };
    let Some(token) = fetch_bearer_token(client, &challenge).await? else {
        return Ok(None);
    };
    let authed = client
        .get(url)
        .header("Accept", MANIFEST_ACCEPT)
        .bearer_auth(token)
        .send()
        .await?;
    if authed.status().is_success() {
        Ok(Some(authed))
    } else {
        Ok(None)
    }
}

/// The pieces of a `WWW-Authenticate: Bearer ...` challenge we need to
/// mint a token.
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl BearerChallenge {
    /// Parse `Bearer realm="https://ghcr.io/token",service="ghcr.io",
    /// scope="repository:owner/name:pull"`. Returns None if it isn't a
    /// Bearer challenge or has no `realm` (without which we can't fetch
    /// a token).
    fn parse(header: &str) -> Option<Self> {
        let rest = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer "))?;
        let mut realm = None;
        let mut service = None;
        let mut scope = None;
        // Params are comma-separated `key="value"` pairs. Values are
        // quoted and never contain a quote themselves in registry
        // challenges, so a simple split on `="` / `"` is enough.
        for part in rest.split(',') {
            let part = part.trim();
            let Some((key, val)) = part.split_once('=') else {
                continue;
            };
            let val = val.trim().trim_matches('"').to_string();
            match key.trim() {
                "realm" => realm = Some(val),
                "service" => service = Some(val),
                "scope" => scope = Some(val),
                _ => {}
            }
        }
        Some(Self {
            realm: realm?,
            service,
            scope,
        })
    }
}

/// Exchange a Bearer challenge for an anonymous token at `realm`. The
/// token endpoint returns JSON with either `token` or `access_token`.
async fn fetch_bearer_token(
    client: &reqwest::Client,
    challenge: &BearerChallenge,
) -> Result<Option<String>> {
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(service) = &challenge.service {
        query.push(("service", service.as_str()));
    }
    if let Some(scope) = &challenge.scope {
        query.push(("scope", scope.as_str()));
    }
    let resp = client.get(&challenge.realm).query(&query).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        // An empty token would just produce `Authorization: Bearer ` and
        // a guaranteed re-401; treat it as "no usable token".
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(token)
}

struct ParsedRef {
    host: String,
    name: String,
    tag: String,
    is_insecure: bool,
}

impl ParsedRef {
    fn parse(image_ref: &str) -> Option<Self> {
        // tag
        let (without_tag, tag) = match image_ref.rsplit_once(':') {
            // The colon in `localhost:5000/...` is the port, not a tag.
            // A real tag colon never has a `/` after it.
            Some((before, after)) if !after.contains('/') => (before, after.to_string()),
            _ => (image_ref, "latest".to_string()),
        };

        // host / name
        let (host, name) = match without_tag.split_once('/') {
            Some((h, n)) if h.contains('.') || h.contains(':') || h == "localhost" => {
                (h.to_string(), n.to_string())
            }
            _ => ("registry-1.docker.io".to_string(), without_tag.to_string()),
        };

        let is_insecure = host.starts_with("localhost") || host.starts_with("127.");

        Some(Self {
            host,
            name,
            tag,
            is_insecure,
        })
    }
}

fn short(digest: &str) -> String {
    digest.trim_start_matches("sha256:").chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_localhost_with_port() {
        let p = ParsedRef::parse("localhost:5000/agent-vm-template:latest").unwrap();
        assert_eq!(p.host, "localhost:5000");
        assert_eq!(p.name, "agent-vm-template");
        assert_eq!(p.tag, "latest");
        assert!(p.is_insecure);
    }

    #[test]
    fn parses_dockerhub_short() {
        let p = ParsedRef::parse("alpine").unwrap();
        assert_eq!(p.host, "registry-1.docker.io");
        assert_eq!(p.name, "alpine");
        assert_eq!(p.tag, "latest");
        assert!(!p.is_insecure);
    }

    #[test]
    fn parses_ghcr_explicit_tag() {
        let p = ParsedRef::parse("ghcr.io/wirenboard/agent-vm-template:v1").unwrap();
        assert_eq!(p.host, "ghcr.io");
        assert_eq!(p.name, "wirenboard/agent-vm-template");
        assert_eq!(p.tag, "v1");
        assert!(!p.is_insecure);
    }

    #[test]
    fn parses_ghcr_bearer_challenge() {
        // The exact header ghcr.io returns for an anonymous manifest GET.
        let c = BearerChallenge::parse(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:wirenboard/agent-vm-template:pull""#,
        )
        .unwrap();
        assert_eq!(c.realm, "https://ghcr.io/token");
        assert_eq!(c.service.as_deref(), Some("ghcr.io"));
        assert_eq!(
            c.scope.as_deref(),
            Some("repository:wirenboard/agent-vm-template:pull")
        );
    }

    #[test]
    fn bearer_challenge_realm_only() {
        // service/scope are optional; realm is the one hard requirement.
        let c = BearerChallenge::parse(r#"Bearer realm="https://auth.docker.io/token""#).unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert!(c.service.is_none());
        assert!(c.scope.is_none());
    }

    #[test]
    fn non_bearer_challenge_rejected() {
        // Basic-auth challenge (or a Bearer challenge missing realm) is
        // unusable — we can't mint a token, so parse returns None.
        assert!(BearerChallenge::parse(r#"Basic realm="registry""#).is_none());
        assert!(BearerChallenge::parse(r#"Bearer service="ghcr.io""#).is_none());
    }
}
