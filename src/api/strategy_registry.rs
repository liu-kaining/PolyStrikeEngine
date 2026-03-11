//! Per-market strategy task registry using `CancellationToken` for graceful shutdown.
//! Used by POST /strategy/start (register token) and POST /strategy/stop (cancel by token_id).

use std::sync::Arc;

use dashmap::{mapref::entry::Entry, DashMap};
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
        match self.cancel_tokens.entry(token_id) {
            Entry::Occupied(_) => None,
            Entry::Vacant(v) => {
                let token = CancellationToken::new();
                let child = token.clone();
                v.insert(token);
                Some(child)
            }
        }
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

    /// Remove entry for `token_id` without cancelling. Call when the engine task exits on its own
    /// (e.g. WS stream error) to avoid zombie registry entries.
    pub fn deregister(&self, token_id: &str) {
        self.cancel_tokens.remove(token_id);
    }

    /// Return a clone of the cancellation token for `token_id` if registered (e.g. for child tasks).
    #[allow(dead_code)]
    pub fn get_token(&self, token_id: &str) -> Option<CancellationToken> {
        self.cancel_tokens.get(token_id).map(|t| t.clone())
    }

    /// Number of active strategies being monitored.
    pub fn len(&self) -> usize {
        self.cancel_tokens.len()
    }
}
