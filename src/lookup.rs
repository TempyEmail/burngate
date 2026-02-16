use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tracing::{debug, error};

use crate::config::{CheckMode, Config};

/// Handles Redis-based mailbox existence checks.
#[derive(Clone)]
pub struct MailboxLookup {
    conn: ConnectionManager,
    key_pattern: String,
    set_name: String,
    check_mode: CheckMode,
}

impl MailboxLookup {
    pub fn new(conn: ConnectionManager, config: &Config) -> Self {
        Self {
            conn,
            key_pattern: config.redis_key_pattern.clone(),
            set_name: config.redis_set_name.clone(),
            check_mode: config.redis_check_mode.clone(),
        }
    }

    /// Build the Redis key for a given address using the configured pattern.
    fn key_for(&self, address: &str) -> String {
        self.key_pattern
            .replace("{address}", &address.to_lowercase())
    }

    /// Check if a mailbox is currently active (EXISTS on the key).
    pub async fn is_active(&self, address: &str) -> Result<bool, redis::RedisError> {
        let key = self.key_for(address);
        let mut conn = self.conn.clone();
        let exists: bool = conn.exists(&key).await?;
        debug!(address = address, key = %key, exists = exists, "mailbox active check");
        Ok(exists)
    }

    /// Check if an address exists in the configured Redis SET.
    pub async fn is_known(&self, address: &str) -> Result<bool, redis::RedisError> {
        let mut conn = self.conn.clone();
        let exists: bool = conn
            .sismember(&self.set_name, address.to_lowercase())
            .await?;
        debug!(address = address, set = %self.set_name, exists = exists, "mailbox known check");
        Ok(exists)
    }

    /// Check if the mailbox should accept mail, respecting the configured check mode.
    pub async fn should_accept(&self, address: &str) -> bool {
        match self.check_mode {
            CheckMode::KeyOnly => self.check_key(address).await,
            CheckMode::SetOnly => self.check_set(address).await,
            CheckMode::Both => {
                if self.check_key(address).await {
                    return true;
                }
                // Fallback: check the set. Catches mail arriving in the brief
                // window between mailbox expiry and the sender's retry.
                self.check_set(address).await
            }
        }
    }

    async fn check_key(&self, address: &str) -> bool {
        match self.is_active(address).await {
            Ok(exists) => exists,
            Err(e) => {
                error!(error = %e, address = address, "redis error on key check");
                false // fail closed
            }
        }
    }

    async fn check_set(&self, address: &str) -> bool {
        if self.set_name.is_empty() {
            return false;
        }
        match self.is_known(address).await {
            Ok(known) => known,
            Err(e) => {
                error!(error = %e, address = address, "redis error on set check, rejecting");
                false // fail closed
            }
        }
    }
}
