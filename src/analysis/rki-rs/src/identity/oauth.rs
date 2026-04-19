use async_trait::async_trait;
use crate::identity::{Credential, IdentityProvider};

/// Kimi OAuth device flow provider.
pub struct KimiOAuthProvider {
    client_id: String,
    auth_url: String,
    token_url: String,
}

impl KimiOAuthProvider {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            auth_url: "https://auth.kimi.com/device".to_string(),
            token_url: "https://auth.kimi.com/token".to_string(),
        }
    }
}

#[async_trait]
impl IdentityProvider for KimiOAuthProvider {
    fn name(&self) -> &str {
        "kimi_oauth"
    }

    async fn authenticate(&self) -> anyhow::Result<Credential> {
        let client = reqwest::Client::new();
        let resp = client
            .post(&self.auth_url)
            .form(&[("client_id", &self.client_id)])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("OAuth device flow initiation failed: {}", resp.text().await?);
        }

        let data: serde_json::Value = resp.json().await?;
        let device_code = data["device_code"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing device_code"))?;
        let user_code = data["user_code"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing user_code"))?;
        let verification_uri = data["verification_uri"]
            .as_str()
            .unwrap_or("https://auth.kimi.com/activate");
        let expires_in = data["expires_in"].as_u64().unwrap_or(600);
        let interval = data["interval"].as_u64().unwrap_or(5);

        eprintln!(
            "Please visit {} and enter code: {} (expires in {}s)",
            verification_uri, user_code, expires_in
        );

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(expires_in);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

            if start.elapsed() > timeout {
                anyhow::bail!("OAuth device flow timed out");
            }

            let poll_resp = client
                .post(&self.token_url)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", device_code),
                    ("client_id", &self.client_id),
                ])
                .send()
                .await?;

            let poll_data: serde_json::Value = poll_resp.json().await?;

            if let Some(access_token) = poll_data["access_token"].as_str() {
                let refresh_token = poll_data["refresh_token"].as_str().map(|s| s.to_string());
                let expires_in = poll_data["expires_in"].as_u64().unwrap_or(3600);
                return Ok(Credential {
                    key: "kimi_oauth".to_string(),
                    value: access_token.to_string(),
                    provider: "kimi_oauth".to_string(),
                    expires_at: Some(
                        chrono::Utc::now() + chrono::Duration::seconds(expires_in as i64),
                    ),
                    refresh_token,
                });
            }

            if let Some(error) = poll_data["error"].as_str()
                && error != "authorization_pending" {
                    anyhow::bail!("OAuth error: {}", error);
                }
        }
    }

    async fn refresh(&self, credential: &Credential) -> anyhow::Result<Credential> {
        let refresh_token = credential
            .refresh_token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No refresh token available"))?;

        let client = reqwest::Client::new();
        let resp = client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &self.client_id),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Token refresh failed: {}", resp.text().await?);
        }

        let data: serde_json::Value = resp.json().await?;
        let access_token = data["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing access_token in refresh response"))?;
        let new_refresh = data["refresh_token"].as_str().map(|s| s.to_string());
        let expires_in = data["expires_in"].as_u64().unwrap_or(3600);

        Ok(Credential {
            key: credential.key.clone(),
            value: access_token.to_string(),
            provider: credential.provider.clone(),
            expires_at: Some(
                chrono::Utc::now() + chrono::Duration::seconds(expires_in as i64),
            ),
            refresh_token: new_refresh.or_else(|| credential.refresh_token.clone()),
        })
    }

    async fn revoke(&self, _credential: &Credential) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_provider_name() {
        let provider = KimiOAuthProvider::new("test_client");
        assert_eq!(provider.name(), "kimi_oauth");
    }

    #[test]
    fn test_oauth_provider_urls() {
        let provider = KimiOAuthProvider::new("my_client");
        assert_eq!(provider.client_id, "my_client");
        assert!(provider.auth_url.contains("kimi.com"));
        assert!(provider.token_url.contains("kimi.com"));
    }

    #[tokio::test]
    async fn test_oauth_revoke_is_noop() {
        let provider = KimiOAuthProvider::new("test");
        let cred = crate::identity::Credential {
            key: "kimi".to_string(),
            value: "token".to_string(),
            provider: "kimi_oauth".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        assert!(provider.revoke(&cred).await.is_ok());
    }

    #[test]
    fn test_oauth_provider_new_with_string() {
        let provider = KimiOAuthProvider::new(String::from("client_123"));
        assert_eq!(provider.client_id, "client_123");
    }
}
