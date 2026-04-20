//! Identity and credential management for LLM providers.
//!
//! `IdentityManager` abstracts OAuth, API keys, and environment variables.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub mod oauth;

/// A resolved credential (e.g., API key, OAuth token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub key: String,
    pub value: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// Abstract storage for credentials.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Credential>>;
    async fn set(&self, key: &str, credential: &Credential) -> anyhow::Result<()>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
}

/// Reads credentials from environment variables. Never persists.
pub struct EnvCredentialStore {
    prefix: String,
}

impl EnvCredentialStore {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

#[async_trait]
impl CredentialStore for EnvCredentialStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Credential>> {
        let var_name = format!("{}{}", self.prefix, key.to_uppercase());
        Ok(std::env::var(&var_name)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|value| Credential {
                key: key.to_string(),
                value,
                provider: "env".to_string(),
                expires_at: None,
                refresh_token: None,
            }))
    }

    async fn set(&self, _key: &str, _credential: &Credential) -> anyhow::Result<()> {
        anyhow::bail!("EnvCredentialStore is read-only")
    }

    async fn delete(&self, _key: &str) -> anyhow::Result<()> {
        anyhow::bail!("EnvCredentialStore is read-only")
    }
}

/// File-backed credential store with atomic writes and restricted permissions.
pub struct FileCredentialStore {
    base_dir: std::path::PathBuf,
}

impl FileCredentialStore {
    pub fn new(base_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(base_dir)?;
        Ok(Self {
            base_dir: base_dir.to_path_buf(),
        })
    }

    fn path(&self, key: &str) -> std::path::PathBuf {
        self.base_dir.join(format!("{}.json", key))
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Credential>> {
        let path = self.path(key);
        if !path.exists() {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let cred: Credential = serde_json::from_str(&content)?;
        Ok(Some(cred))
    }

    async fn set(&self, key: &str, credential: &Credential) -> anyhow::Result<()> {
        let path = self.path(key);
        let temp = path.with_extension("tmp");
        let content = serde_json::to_string_pretty(credential)?;
        tokio::fs::write(&temp, content).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&temp)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&temp, perms)?;
        }
        tokio::fs::rename(&temp, &path).await?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.path(key);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }
}

/// Chained credential store: tries multiple stores in order.
pub struct ChainedCredentialStore {
    stores: Vec<Box<dyn CredentialStore>>,
}

impl ChainedCredentialStore {
    pub fn new(stores: Vec<Box<dyn CredentialStore>>) -> Self {
        Self { stores }
    }
}

#[async_trait]
impl CredentialStore for ChainedCredentialStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Credential>> {
        for store in &self.stores {
            if let Ok(Some(cred)) = store.get(key).await {
                return Ok(Some(cred));
            }
        }
        Ok(None)
    }

    async fn set(&self, key: &str, credential: &Credential) -> anyhow::Result<()> {
        // Write to the first writable store
        for store in &self.stores {
            if store.set(key, credential).await.is_ok() {
                return Ok(());
            }
        }
        anyhow::bail!("No writable credential store available")
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        for store in &self.stores {
            let _ = store.delete(key).await;
        }
        Ok(())
    }
}

/// Identity provider protocol for authentication.
#[async_trait]
#[allow(dead_code)]
pub trait IdentityProvider: Send + Sync {
    fn name(&self) -> &str;
    /// Authenticate and return a credential.
    async fn authenticate(&self) -> anyhow::Result<Credential>;
    /// Refresh an existing credential.
    async fn refresh(&self, credential: &Credential) -> anyhow::Result<Credential>;
    /// Revoke a credential.
    async fn revoke(&self, credential: &Credential) -> anyhow::Result<()>;
}

/// Simple API key provider that reads from a credential store.
pub struct ApiKeyProvider {
    name: String,
    store: Box<dyn CredentialStore>,
    key_name: String,
}

impl ApiKeyProvider {
    pub fn new(
        name: impl Into<String>,
        store: Box<dyn CredentialStore>,
        key_name: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            store,
            key_name: key_name.into(),
        }
    }
}

#[async_trait]
impl IdentityProvider for ApiKeyProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn authenticate(&self) -> anyhow::Result<Credential> {
        self.store
            .get(&self.key_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("API key '{}' not found", self.key_name))
    }

    async fn refresh(&self, _credential: &Credential) -> anyhow::Result<Credential> {
        // API keys don't refresh; re-authenticate
        self.authenticate().await
    }

    async fn revoke(&self, _credential: &Credential) -> anyhow::Result<()> {
        self.store.delete(&self.key_name).await
    }
}

/// Identity manager that resolves credentials for providers.
pub struct IdentityManager {
    providers: HashMap<String, Box<dyn IdentityProvider>>,
    store: Box<dyn CredentialStore>,
}

impl IdentityManager {
    pub fn new(store: Box<dyn CredentialStore>) -> Self {
        Self {
            providers: HashMap::new(),
            store,
        }
    }

    pub fn register_provider(
        &mut self,
        name: impl Into<String>,
        provider: Box<dyn IdentityProvider>,
    ) {
        self.providers.insert(name.into(), provider);
    }

    #[allow(dead_code)]
    pub async fn resolve(&self, provider_name: &str) -> anyhow::Result<Credential> {
        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown identity provider: {}", provider_name))?;
        provider.authenticate().await
    }

    pub async fn get_key(&self, key_name: &str) -> anyhow::Result<Option<Credential>> {
        self.store.get(key_name).await
    }

    /// Build a default identity manager with env + file stores.
    pub fn default_for_kimi() -> anyhow::Result<Self> {
        let env_store = Box::new(EnvCredentialStore::new(""));
        let file_store = Box::new(FileCredentialStore::new(
            &dirs::home_dir()
                .unwrap_or_default()
                .join(".kimi/credentials"),
        )?);
        let chained = Box::new(ChainedCredentialStore::new(vec![env_store, file_store]));

        let mut manager = Self::new(chained);

        // Register default providers
        let openai_store: Box<dyn CredentialStore> = {
            let env = Box::new(EnvCredentialStore::new(""));
            let file = Box::new(FileCredentialStore::new(
                &dirs::home_dir()
                    .unwrap_or_default()
                    .join(".kimi/credentials"),
            )?);
            Box::new(ChainedCredentialStore::new(vec![env, file]))
        };
        manager.register_provider(
            "openai",
            Box::new(ApiKeyProvider::new(
                "openai",
                openai_store,
                "OPENAI_API_KEY",
            )),
        );

        let anthropic_store: Box<dyn CredentialStore> = {
            let env = Box::new(EnvCredentialStore::new(""));
            let file = Box::new(FileCredentialStore::new(
                &dirs::home_dir()
                    .unwrap_or_default()
                    .join(".kimi/credentials"),
            )?);
            Box::new(ChainedCredentialStore::new(vec![env, file]))
        };
        manager.register_provider(
            "anthropic",
            Box::new(ApiKeyProvider::new(
                "anthropic",
                anthropic_store,
                "ANTHROPIC_API_KEY",
            )),
        );

        Ok(manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_env_credential_store() {
        unsafe {
            std::env::set_var("TEST_API_KEY", "secret123");
        }
        let store = EnvCredentialStore::new("TEST_");
        let cred = store.get("API_KEY").await.unwrap();
        assert!(cred.is_some());
        assert_eq!(cred.unwrap().value, "secret123");
    }

    #[tokio::test]
    async fn test_file_credential_store_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(temp.path()).unwrap();
        let cred = Credential {
            key: "test_key".to_string(),
            value: "test_value".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        store.set("test_key", &cred).await.unwrap();
        let loaded = store.get("test_key").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().value, "test_value");
    }

    #[tokio::test]
    async fn test_chained_store_reads_first_hit() {
        let env_store = Box::new(EnvCredentialStore::new(""));
        let temp = tempfile::tempdir().unwrap();
        let file_store = Box::new(FileCredentialStore::new(temp.path()).unwrap());
        let chained = ChainedCredentialStore::new(vec![env_store, file_store]);

        // Write to file store
        let cred = Credential {
            key: "my_key".to_string(),
            value: "from_file".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        chained.set("my_key", &cred).await.unwrap();

        // Read back
        let result = chained.get("my_key").await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_api_key_provider() {
        let temp = tempfile::tempdir().unwrap();
        let store = Box::new(FileCredentialStore::new(temp.path()).unwrap());
        let provider = ApiKeyProvider::new("test", store, "api_key");

        // Not found initially
        assert!(provider.authenticate().await.is_err());

        // Store a credential
        let cred = Credential {
            key: "api_key".to_string(),
            value: "sk-test".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        provider.store.set("api_key", &cred).await.unwrap();

        // Now found
        let resolved = provider.authenticate().await.unwrap();
        assert_eq!(resolved.value, "sk-test");
    }

    #[tokio::test]
    async fn test_file_credential_store_delete() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(temp.path()).unwrap();
        let cred = Credential {
            key: "del_key".to_string(),
            value: "val".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        store.set("del_key", &cred).await.unwrap();
        assert!(store.get("del_key").await.unwrap().is_some());

        store.delete("del_key").await.unwrap();
        assert!(store.get("del_key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_identity_manager_get_key() {
        let temp = tempfile::tempdir().unwrap();
        let store = Box::new(FileCredentialStore::new(temp.path()).unwrap());
        let mut manager = IdentityManager::new(store);

        let provider = Box::new(ApiKeyProvider::new(
            "test",
            Box::new(FileCredentialStore::new(temp.path()).unwrap()),
            "my_key",
        ));
        manager.register_provider("test", provider);

        let key = manager.get_key("my_key").await.unwrap();
        assert!(key.is_none());
    }
}
