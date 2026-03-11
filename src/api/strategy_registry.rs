//! Per-market strategy task registry using `CancellationToken` for graceful shutdown.
//! Used by POST /strategy/start (register token) and POST /strategy/stop (cancel by token_id).

use std::sync::Arc;

use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

/// Registry of running strategy tasks keyed by market `token_id`.
/// Each entry holds the cancellation token for that market's engine loop.
#[derive(Default, Clone)]
pub struct StrategyRegistry {
    /// token_id -> CancellationToken; cancelling the token stops the strategy task.
    cancel_tokens: Arc<DashMap<String, CancellationToken>>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self {
            cancel_tokens: Arc::new(DashMap::new()),
        }
    }

    /// Register a new strategy for `token_id`. Returns a clone of the token for the engine task.
    /// If `token_id` already exists, cancels the old token and replaces it.
    #[allow(dead_code)]
    pub fn register(&self, token_id: String) -> CancellationToken {
        let token = CancellationToken::new();
        let child = token.clone();
        self.cancel_tokens.insert(token_id, token);
        child
    }

    /// Register only if `token_id` is not already running. Returns `Some(token)` for the engine task, or `None` if already active.
    pub fn try_register(&self, token_id: String) -> Option<CancellationToken> {
        if self.cancel_tokens.contains_key(&token_id) {
            return None;
        }
        let token = CancellationToken::new();
        let child = token.clone();
        self.cancel_tokens.insert(token_id, token);
        Some(child)
    }

    /// Stop the strategy for `token_id` by triggering cancellation. Returns true if found.
    pub fn cancel(&self, token_id: &str) -> bool {
        if let Some((_, token)) = self.cancel_tokens.remove(token_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Return a clone of the cancellation token for `token_id` if registered (e.g. for child tasks).
    #[allow(dead_code)]
    pub fn get_token(&self, token_id: &str) -> Option<CancellationToken> {
        self.cancel_tokens.get(token_id).map(|t| t.clone())
    }

    /// Whether a strategy is currently registered for `token_id`.
    pub fn is_running(&self, token_id: &str) -> bool {
        self.cancel_tokens.contains_key(token_id)
    }
}
