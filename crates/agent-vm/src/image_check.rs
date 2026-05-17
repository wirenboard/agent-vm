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
use microsandbox::Image;

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

/// Compare the local image cache against the registry. Returns `Ok(None)`
/// when we can't decide (registry unreachable, malformed reference, etc.).
pub async fn check_for_update(image_ref: &str) -> Result<Option<UpdateState>> {
    let cached = cached_manifest_digest(image_ref).await;
    let Some(parsed) = ParsedRef::parse(image_ref) else {
        return Ok(None);
    };
    let remote = match remote_manifest_digest(&parsed).await {
        Ok(Some(d)) => d,
        Ok(None) | Err(_) => return Ok(None),
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

async fn cached_manifest_digest(image_ref: &str) -> Option<String> {
    let handle = Image::get(image_ref).await.ok()?;
    handle.manifest_digest().map(|s| s.to_string())
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
    let resp = client
        .get(&url)
        .header(
            "Accept",
            concat!(
                "application/vnd.docker.distribution.manifest.v2+json,",
                "application/vnd.docker.distribution.manifest.list.v2+json,",
                "application/vnd.oci.image.manifest.v1+json,",
                "application/vnd.oci.image.index.v1+json",
            ),
        )
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
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
        let p = ParsedRef::parse("localhost:5000/agent-vm:latest").unwrap();
        assert_eq!(p.host, "localhost:5000");
        assert_eq!(p.name, "agent-vm");
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
        let p = ParsedRef::parse("ghcr.io/wirenboard/agent-vm:v1").unwrap();
        assert_eq!(p.host, "ghcr.io");
        assert_eq!(p.name, "wirenboard/agent-vm");
        assert_eq!(p.tag, "v1");
        assert!(!p.is_insecure);
    }
}
