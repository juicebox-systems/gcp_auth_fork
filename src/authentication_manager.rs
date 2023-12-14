use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, info, warn};

use crate::custom_service_account::CustomServiceAccount;
use crate::default_authorized_user::ConfigDefaultCredentials;
use crate::default_service_account::MetadataServiceAccount;
use crate::error::Error;
use crate::gcloud_authorized_user::GCloudAuthorizedUser;
use crate::types::{self, HyperClient, Token};

#[async_trait]
pub(crate) trait ServiceAccount: Send + Sync {
    async fn project_id(&self, client: &HyperClient) -> Result<String, Error>;
    fn get_token(&self, scopes: &[&str]) -> Option<Token>;
    async fn refresh_token(&self, client: &HyperClient, scopes: &[&str]) -> Result<Token, Error>;
}

/// Authentication manager is responsible for caching and obtaining credentials for the required
/// scope
///
/// Construct the authentication manager with [`AuthenticationManager::new()`] or by creating
/// a [`CustomServiceAccount`], then converting it into an `AuthenticationManager` using the `From`
/// impl.
#[derive(Clone)]
pub struct AuthenticationManager(Arc<AuthManagerInner>);

struct AuthManagerInner {
    client: HyperClient,
    service_account: Box<dyn ServiceAccount>,
    refresh_mutex: Arc<Mutex<()>>,
}

impl AuthenticationManager {
    /// Finds a service account provider to get authentication tokens from
    ///
    /// Tries the following approaches, in order:
    ///
    /// 1. Check if the `GOOGLE_APPLICATION_CREDENTIALS` environment variable if set;
    ///    if so, use a custom service account as the token source.
    /// 2. Look for credentials in `.config/gcloud/application_default_credentials.json`;
    ///    if found, use these credentials to request refresh tokens.
    /// 3. Send a HTTP request to the internal metadata server to retrieve a token;
    ///    if it succeeds, use the default service account as the token source.
    /// 4. Check if the `gcloud` tool is available on the `PATH`; if so, use the
    ///    `gcloud auth print-access-token` command as the token source.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn new() -> Result<Self, Error> {
        tracing::debug!("Initializing gcp_auth");
        if let Some(service_account) = CustomServiceAccount::from_env()? {
            return Ok(service_account.into());
        }

        let client = types::client();
        let default_user_error = match ConfigDefaultCredentials::new(&client).await {
            Ok(service_account) => {
                tracing::debug!("Using ConfigDefaultCredentials");
                return Ok(Self::build(client, service_account));
            }
            Err(e) => e,
        };

        let default_service_error = match MetadataServiceAccount::new(&client).await {
            Ok(service_account) => {
                tracing::debug!("Using MetadataServiceAccount");
                return Ok(Self::build(client, service_account));
            }
            Err(e) => e,
        };

        let gcloud_error = match GCloudAuthorizedUser::new().await {
            Ok(service_account) => {
                tracing::debug!("Using GCloudAuthorizedUser");
                return Ok(Self::build(client, service_account));
            }
            Err(e) => e,
        };

        Err(Error::NoAuthMethod(
            Box::new(gcloud_error),
            Box::new(default_service_error),
            Box::new(default_user_error),
        ))
    }

    fn build(client: HyperClient, service_account: impl ServiceAccount + 'static) -> Self {
        Self(Arc::new(AuthManagerInner {
            client,
            service_account: Box::new(service_account),
            refresh_mutex: Arc::new(Mutex::new(())),
        }))
    }

    /// Requests Bearer token for the provided scope
    ///
    /// Token can be used in the request authorization header in format "Bearer {token}"
    pub async fn get_token(&self, scopes: &'static [&'static str]) -> Result<Token, Error> {
        let token = self.0.service_account.get_token(scopes);

        if let Some(token) = token.filter(|token| !token.has_expired()) {
            let valid_for = token
                .expires_at()
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            if valid_for < Duration::from_secs(60) {
                debug!(?valid_for, "gcp_auth token expires soon!");

                match self.0.refresh_mutex.clone().try_lock_owned() {
                    Err(_) => {
                        // failed to take the lock, something is already doing a refresh.
                    }
                    Ok(guard) => {
                        let inner = self.clone();
                        tokio::spawn(async move {
                            inner.background_refresh(scopes, guard).await;
                        });
                    }
                }
            }
            return Ok(token);
        }

        warn!("starting inline refresh of gcp auth token");
        let _guard = self.0.refresh_mutex.lock().await;

        // Check if refresh happened while we were waiting.
        let token = self.0.service_account.get_token(scopes);
        if let Some(token) = token.filter(|token| !token.has_expired()) {
            return Ok(token);
        }

        self.0
            .service_account
            .refresh_token(&self.0.client, scopes)
            .await
    }

    async fn background_refresh(
        &self,
        scopes: &'static [&'static str],
        _lock: OwnedMutexGuard<()>,
    ) {
        info!("gcp_auth starting background refresh of auth token");
        match self
            .0
            .service_account
            .refresh_token(&self.0.client, scopes)
            .await
        {
            Ok(t) => {
                info!(valid_for=?t.expires_at().duration_since(SystemTime::now()), "gcp auth completed background token refresh")
            }
            Err(err) => warn!(?err, "gcp_auth background token refresh failed"),
        }
    }

    /// Request the project ID for the authenticating account
    ///
    /// This is only available for service account-based authentication methods.
    pub async fn project_id(&self) -> Result<String, Error> {
        self.0.service_account.project_id(&self.0.client).await
    }
}

impl From<CustomServiceAccount> for AuthenticationManager {
    fn from(service_account: CustomServiceAccount) -> Self {
        Self::build(types::client(), service_account)
    }
}
