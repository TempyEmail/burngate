use std::future::Future;

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use tracing::{debug, error};

use crate::config::{CheckMode, Config};

/// Result of a mailbox existence check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The mailbox exists: accept the recipient.
    Accept,
    /// The mailbox does not exist: reject permanently (5xx).
    Reject,
    /// The lookup failed (e.g. Redis unreachable): reject temporarily (4xx)
    /// so the sender retries instead of bouncing the mail.
    TempFail,
}

/// Anything that can decide whether a mailbox accepts mail.
pub trait MailboxCheck: Send + Sync {
    /// Check an (already lowercased) address.
    fn check(&self, address: &str) -> impl Future<Output = Verdict> + Send;
}

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
    pub async fn should_accept(&self, address: &str) -> Verdict {
        match self.check_mode {
            CheckMode::KeyOnly => self.check_key(address).await,
            CheckMode::SetOnly => self.check_set(address).await,
            CheckMode::Both => match self.check_key(address).await {
                Verdict::Accept => Verdict::Accept,
                // Fallback: check the set. Catches mail arriving in the brief
                // window between mailbox expiry and the sender's retry.
                key_verdict => match self.check_set(address).await {
                    Verdict::Accept => Verdict::Accept,
                    // If either lookup failed, we don't know for sure: tempfail.
                    set_verdict
                        if key_verdict == Verdict::TempFail || set_verdict == Verdict::TempFail =>
                    {
                        Verdict::TempFail
                    }
                    _ => Verdict::Reject,
                },
            },
        }
    }

    async fn check_key(&self, address: &str) -> Verdict {
        match self.is_active(address).await {
            Ok(true) => Verdict::Accept,
            Ok(false) => Verdict::Reject,
            Err(e) => {
                error!(error = %e, address = address, "redis error on key check");
                Verdict::TempFail
            }
        }
    }

    async fn check_set(&self, address: &str) -> Verdict {
        if self.set_name.is_empty() {
            return Verdict::Reject;
        }
        match self.is_known(address).await {
            Ok(true) => Verdict::Accept,
            Ok(false) => Verdict::Reject,
            Err(e) => {
                error!(error = %e, address = address, "redis error on set check");
                Verdict::TempFail
            }
        }
    }
}

impl MailboxCheck for MailboxLookup {
    fn check(&self, address: &str) -> impl Future<Output = Verdict> + Send {
        self.should_accept(address)
    }
}
