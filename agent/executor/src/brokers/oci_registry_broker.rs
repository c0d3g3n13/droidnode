use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use tracing::{debug, instrument};

use crate::error::{DroidError, Result};
use crate::models::{Digest, ImageConfig, ImageRef, Manifest};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait OciRegistryBroker: Send + Sync {
    async fn fetch_manifest(&self, image_ref: &ImageRef) -> Result<Manifest>;
    async fn fetch_layer(&self, image_ref: &ImageRef, digest: &Digest) -> Result<Bytes>;
    async fn fetch_config(&self, image_ref: &ImageRef, digest: &Digest) -> Result<ImageConfig>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct OciRegistryBrokerImpl {
    client: Client,
}

impl OciRegistryBrokerImpl {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("droidnode/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    // Attempt the request; on 401 fetch a bearer token and retry once.
    async fn authenticated_get(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self.client.get(url).send().await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            let token = self.resolve_token(&resp).await?;
            let resp2 = self
                .client
                .get(url)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .send()
                .await?;
            return Ok(resp2);
        }

        Ok(resp)
    }

    // Parse the WWW-Authenticate header and exchange it for a bearer token.
    async fn resolve_token(&self, resp: &reqwest::Response) -> Result<String> {
        let www_auth = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let realm = extract_challenge_param(&www_auth, "realm")
            .ok_or_else(|| DroidError::OciRegistry("missing realm in WWW-Authenticate".into()))?;
        let service = extract_challenge_param(&www_auth, "service").unwrap_or_default();
        let scope = extract_challenge_param(&www_auth, "scope").unwrap_or_default();

        let token_url = format!("{realm}?service={service}&scope={scope}");
        debug!(%token_url, "fetching OCI auth token");

        #[derive(Deserialize)]
        struct TokenResp {
            token: Option<String>,
            access_token: Option<String>,
        }

        let tok: TokenResp = self.client.get(&token_url).send().await?.json().await?;
        tok.token
            .or(tok.access_token)
            .ok_or_else(|| DroidError::OciRegistry("token response contained no token".into()))
    }
}

fn extract_challenge_param<'a>(header: &'a str, key: &str) -> Option<String> {
    // Format: Bearer realm="...",service="...",scope="..."
    let search = format!("{key}=\"");
    let start = header.find(&search)? + search.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_string())
}

#[async_trait]
impl OciRegistryBroker for OciRegistryBrokerImpl {
    #[instrument(skip(self), fields(image = %image_ref.repository))]
    async fn fetch_manifest(&self, image_ref: &ImageRef) -> Result<Manifest> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            image_ref.registry_url(),
            image_ref.repository,
            image_ref.reference
        );

        let resp = self
            .client
            .get(&url)
            .header(
                header::ACCEPT,
                "application/vnd.oci.image.manifest.v1+json, \
                 application/vnd.docker.distribution.manifest.v2+json",
            )
            .send()
            .await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            let token = self.resolve_token(&resp).await?;
            let resp2 = self
                .client
                .get(&url)
                .header(
                    header::ACCEPT,
                    "application/vnd.oci.image.manifest.v1+json, \
                     application/vnd.docker.distribution.manifest.v2+json",
                )
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .send()
                .await?;

            if !resp2.status().is_success() {
                return Err(DroidError::OciRegistry(format!(
                    "manifest fetch failed: {}",
                    resp2.status()
                )));
            }
            return Ok(resp2.json::<Manifest>().await?);
        }

        if !resp.status().is_success() {
            return Err(DroidError::ImageNotFound(format!(
                "{}:{}",
                image_ref.repository, image_ref.reference
            )));
        }

        Ok(resp.json::<Manifest>().await?)
    }

    #[instrument(skip(self), fields(digest = %digest))]
    async fn fetch_layer(&self, image_ref: &ImageRef, digest: &Digest) -> Result<Bytes> {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            image_ref.registry_url(),
            image_ref.repository,
            digest
        );

        debug!(%url, "fetching layer blob");
        let resp = self.authenticated_get(&url).await?;

        if !resp.status().is_success() {
            return Err(DroidError::OciRegistry(format!(
                "blob fetch failed: {}",
                resp.status()
            )));
        }

        Ok(resp.bytes().await?)
    }

    #[instrument(skip(self), fields(digest = %digest))]
    async fn fetch_config(&self, image_ref: &ImageRef, digest: &Digest) -> Result<ImageConfig> {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            image_ref.registry_url(),
            image_ref.repository,
            digest
        );

        debug!(%url, "fetching image config");
        let resp = self.authenticated_get(&url).await?;

        if !resp.status().is_success() {
            return Err(DroidError::OciRegistry(format!(
                "config fetch failed: {}",
                resp.status()
            )));
        }

        Ok(resp.json::<ImageConfig>().await?)
    }
}
