//! Session manager for multi-user, multi-thread conversation handling.
//!
//! Maps external channel thread IDs to internal UUIDs and manages undo state
//! for each thread.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::agent::session::{Session, Thread, Turn};
use crate::agent::undo::UndoManager;
use crate::history::FjallHistoryStore as Store;

/// Key for mapping external thread IDs to internal ones.
#[derive(Clone, Hash, Eq, PartialEq)]
struct ThreadKey {
    user_id: String,
    channel: String,
    external_thread_id: Option<String>,
}

/// Manages sessions, threads, and undo state for all users.
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<Mutex<Session>>>>,
    thread_map: RwLock<HashMap<ThreadKey, Uuid>>,
    undo_managers: RwLock<HashMap<Uuid, Arc<Mutex<UndoManager>>>>,
    /// Optional Fjall store. When set, fresh sessions rehydrate their
    /// `threads` map from disk so prior conversations survive a gateway
    /// restart. None disables persistence (used in unit tests).
    store: Option<Arc<Store>>,
}

impl SessionManager {
    /// Create a new session manager with no persistence (in-memory only).
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            thread_map: RwLock::new(HashMap::new()),
            undo_managers: RwLock::new(HashMap::new()),
            store: None,
        }
    }

    /// Create a new session manager backed by a Fjall store. Sessions
    /// will rehydrate their thread history from disk on first access.
    pub fn with_store(store: Arc<Store>) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            thread_map: RwLock::new(HashMap::new()),
            undo_managers: RwLock::new(HashMap::new()),
            store: Some(store),
        }
    }

    /// Borrow the underlying Fjall store, if any. Used by the rename +
    /// delete thread handlers to push changes through to disk after
    /// updating the in-memory state.
    pub fn store_ref(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    /// Get or create a session for a user. On first access (cache miss),
    /// rehydrate prior threads from the Fjall store if one is wired.
    pub async fn get_or_create_session(&self, user_id: &str) -> Arc<Mutex<Session>> {
        // Fast path: check if session exists
        {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions.get(user_id) {
                return Arc::clone(session);
            }
        }

        // Slow path: create new session
        let mut sessions = self.sessions.write().await;
        // Double-check after acquiring write lock
        if let Some(session) = sessions.get(user_id) {
            return Arc::clone(session);
        }

        let mut session = Session::new(user_id);

        // Rehydrate prior threads from disk. Each conversation in the
        // store with thread_id == its own id is one Thread; messages
        // (role=user / role=assistant) pair up into Turns by arrival
        // order. We don't bother resurrecting tool_calls or detailed
        // state — the sidebar just needs turn count + content for
        // history display, and any in-flight state was discarded by
        // the prior gateway exit anyway.
        if let Some(store) = &self.store {
            if let Ok(convs) = store.list_conversations_for_user(user_id).await {
                let mut latest_activity: Option<chrono::DateTime<chrono::Utc>> = None;
                let mut latest_thread: Option<Uuid> = None;
                for conv in convs {
                    let Some(thread_id) = conv.thread_id else {
                        continue;
                    };
                    let messages = match store
                        .messages_for_conversation(conv.id)
                        .await
                    {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(
                                "rehydrate messages for {} failed: {}",
                                thread_id,
                                e
                            );
                            continue;
                        }
                    };
                    let mut thread = Thread::new(session.id);
                    thread.id = thread_id;
                    thread.title = conv.title.clone();
                    thread.created_at = conv.created_at;
                    thread.updated_at = conv.last_activity;
                    let mut turn_number: usize = 0;
                    let mut pending_user: Option<(String, chrono::DateTime<chrono::Utc>)> =
                        None;
                    for m in messages {
                        if m.role == "user" {
                            if let Some((ui, _)) = pending_user.take() {
                                // No matching assistant — push the user turn
                                // anyway so the sidebar count is honest.
                                let mut t = Turn::new(turn_number, ui);
                                t.started_at = chrono::Utc::now();
                                thread.turns.push(t);
                                turn_number += 1;
                            }
                            pending_user = Some((m.content, m.created_at));
                        } else if m.role == "assistant" {
                            let (user_input, started_at) = pending_user
                                .take()
                                .unwrap_or_else(|| (String::new(), m.created_at));
                            let mut t = Turn::new(turn_number, user_input);
                            t.started_at = started_at;
                            t.complete(m.content);
                            t.completed_at = Some(m.created_at);
                            thread.turns.push(t);
                            turn_number += 1;
                        }
                    }
                    if let Some((ui, started_at)) = pending_user {
                        let mut t = Turn::new(turn_number, ui);
                        t.started_at = started_at;
                        thread.turns.push(t);
                    }
                    if latest_activity
                        .map(|prev| conv.last_activity > prev)
                        .unwrap_or(true)
                    {
                        latest_activity = Some(conv.last_activity);
                        latest_thread = Some(thread_id);
                    }
                    session.threads.insert(thread_id, thread);
                }
                if let Some(t) = latest_thread {
                    session.active_thread = Some(t);
                }
                if !session.threads.is_empty() {
                    tracing::info!(
                        "rehydrated {} thread(s) for {} from Fjall",
                        session.threads.len(),
                        user_id
                    );
                }
            }
        }

        let session = Arc::new(Mutex::new(session));
        sessions.insert(user_id.to_string(), Arc::clone(&session));
        session
    }

    /// Resolve an external thread ID to an internal thread.
    ///
    /// Returns the session and thread ID. Creates both if they don't exist.
    pub async fn resolve_thread(
        &self,
        user_id: &str,
        channel: &str,
        external_thread_id: Option<&str>,
    ) -> (Arc<Mutex<Session>>, Uuid) {
        let session = self.get_or_create_session(user_id).await;

        let key = ThreadKey {
            user_id: user_id.to_string(),
            channel: channel.to_string(),
            external_thread_id: external_thread_id.map(String::from),
        };

        // Check if we have a mapping
        {
            let thread_map = self.thread_map.read().await;
            if let Some(&thread_id) = thread_map.get(&key) {
                // Verify thread still exists in session
                let sess = session.lock().await;
                if sess.threads.contains_key(&thread_id) {
                    return (Arc::clone(&session), thread_id);
                }
            }
        }

        // Create new thread (always create a new one for a new key)
        let thread_id = {
            let mut sess = session.lock().await;
            let thread = sess.create_thread();
            thread.id
        };

        // Store mapping
        {
            let mut thread_map = self.thread_map.write().await;
            thread_map.insert(key, thread_id);
        }

        // Create undo manager for thread
        {
            let mut undo_managers = self.undo_managers.write().await;
            undo_managers.insert(thread_id, Arc::new(Mutex::new(UndoManager::new())));
        }

        (session, thread_id)
    }

    /// Get undo manager for a thread.
    pub async fn get_undo_manager(&self, thread_id: Uuid) -> Arc<Mutex<UndoManager>> {
        // Fast path
        {
            let managers = self.undo_managers.read().await;
            if let Some(mgr) = managers.get(&thread_id) {
                return Arc::clone(mgr);
            }
        }

        // Create if missing
        let mut managers = self.undo_managers.write().await;
        // Double-check
        if let Some(mgr) = managers.get(&thread_id) {
            return Arc::clone(mgr);
        }

        let mgr = Arc::new(Mutex::new(UndoManager::new()));
        managers.insert(thread_id, Arc::clone(&mgr));
        mgr
    }

    /// Remove sessions that have been idle for longer than the given duration.
    ///
    /// Returns the number of sessions pruned.
    pub async fn prune_stale_sessions(&self, max_idle: std::time::Duration) -> usize {
        let cutoff = chrono::Utc::now() - chrono::TimeDelta::seconds(max_idle.as_secs() as i64);

        // Find stale session user_ids
        let stale_users: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .filter_map(|(user_id, session)| {
                    // Try to lock; skip if contended (someone is actively using it)
                    let sess = session.try_lock().ok()?;
                    if sess.last_active_at < cutoff {
                        Some(user_id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        if stale_users.is_empty() {
            return 0;
        }

        // Collect thread IDs from stale sessions for cleanup
        let mut stale_thread_ids: Vec<Uuid> = Vec::new();
        {
            let sessions = self.sessions.read().await;
            for user_id in &stale_users {
                if let Some(session) = sessions.get(user_id) {
                    if let Ok(sess) = session.try_lock() {
                        stale_thread_ids.extend(sess.threads.keys());
                    }
                }
            }
        }

        // Remove sessions
        let count = {
            let mut sessions = self.sessions.write().await;
            let before = sessions.len();
            for user_id in &stale_users {
                sessions.remove(user_id);
            }
            before - sessions.len()
        };

        // Clean up thread mappings that point to stale sessions
        {
            let mut thread_map = self.thread_map.write().await;
            thread_map.retain(|key, _| !stale_users.contains(&key.user_id));
        }

        // Clean up undo managers for stale threads
        {
            let mut undo_managers = self.undo_managers.write().await;
            for thread_id in &stale_thread_ids {
                undo_managers.remove(thread_id);
            }
        }

        if count > 0 {
            tracing::info!(
                "Pruned {} stale session(s) (idle > {}s)",
                count,
                max_idle.as_secs()
            );
        }

        count
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_or_create_session() {
        let manager = SessionManager::new();

        let session1 = manager.get_or_create_session("user-1").await;
        let session2 = manager.get_or_create_session("user-1").await;

        // Same user should get same session
        assert!(Arc::ptr_eq(&session1, &session2));

        let session3 = manager.get_or_create_session("user-2").await;
        assert!(!Arc::ptr_eq(&session1, &session3));
    }

    #[tokio::test]
    async fn test_resolve_thread() {
        let manager = SessionManager::new();

        let (session1, thread1) = manager.resolve_thread("user-1", "cli", None).await;
        let (session2, thread2) = manager.resolve_thread("user-1", "cli", None).await;

        // Same channel+user should get same thread
        assert!(Arc::ptr_eq(&session1, &session2));
        assert_eq!(thread1, thread2);

        // Different channel should get different thread
        let (_, thread3) = manager.resolve_thread("user-1", "http", None).await;
        assert_ne!(thread1, thread3);
    }

    #[tokio::test]
    async fn test_undo_manager() {
        let manager = SessionManager::new();
        let (_, thread_id) = manager.resolve_thread("user-1", "cli", None).await;

        let undo1 = manager.get_undo_manager(thread_id).await;
        let undo2 = manager.get_undo_manager(thread_id).await;

        assert!(Arc::ptr_eq(&undo1, &undo2));
    }

    #[tokio::test]
    async fn test_prune_stale_sessions() {
        let manager = SessionManager::new();

        // Create two sessions and resolve threads (which updates last_active_at)
        let (_, _thread_id) = manager.resolve_thread("user-active", "cli", None).await;
        let (s2, _thread_id) = manager.resolve_thread("user-stale", "cli", None).await;

        // Backdate the stale session's last_active_at AFTER thread creation
        {
            let mut sess = s2.lock().await;
            sess.last_active_at = chrono::Utc::now() - chrono::TimeDelta::seconds(86400 * 10); // 10 days ago
        }

        // Prune with 7-day timeout
        let pruned = manager
            .prune_stale_sessions(std::time::Duration::from_secs(86400 * 7))
            .await;
        assert_eq!(pruned, 1);

        // Active session should still exist
        let sessions = manager.sessions.read().await;
        assert!(sessions.contains_key("user-active"));
        assert!(!sessions.contains_key("user-stale"));
    }

    #[tokio::test]
    async fn test_prune_no_stale_sessions() {
        let manager = SessionManager::new();
        let _s1 = manager.get_or_create_session("user-1").await;

        // Nothing should be pruned when timeout is long
        let pruned = manager
            .prune_stale_sessions(std::time::Duration::from_secs(86400 * 365))
            .await;
        assert_eq!(pruned, 0);
    }
}
