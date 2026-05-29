//! Fjall-backed history store (replaces the PostgreSQL `Store`).
//!
//! Mirrors the `Store` API — conversations, jobs, actions, LLM calls,
//! estimation snapshots, tool failures — plus the analytics aggregations,
//! computed by scanning partitions. Methods stay `async` (callers await them);
//! the underlying Fjall calls are synchronous.

// Wired in at the C5 swap; tests exercise it now.
#![allow(dead_code)]

use chrono::{DateTime, Utc};
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::BrokenTool;
use crate::context::{ActionRecord, JobContext, JobState};
use crate::error::DatabaseError;
use crate::history::analytics::{
    CategoryHistoryEntry, EstimationAccuracy, JobStats, ToolStats,
};
use crate::history::store::LlmCallRecord;

fn db_err(ctx: &str, e: impl std::fmt::Display) -> DatabaseError {
    DatabaseError::Query(format!("{ctx}: {e}"))
}

/// Summary of one persisted conversation. Returned by
/// `list_conversations_for_user` for the rehydration path.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub thread_id: Option<Uuid>,
    pub channel: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    /// Whether the user has pinned this thread to the top.
    pub pinned: bool,
    /// When it was pinned (None when unpinned). Drives sort within
    /// the pinned section.
    pub pinned_at: Option<DateTime<Utc>>,
    /// Venture this thread belongs to, if any.
    pub venture_id: Option<Uuid>,
}

/// Light-weight view of one Venture for the gateway's list endpoint.
#[derive(Debug, Clone)]
pub struct VentureSummary {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub collapsed: bool,
}

/// One persisted chat turn fragment ("user said X" or "assistant said
/// Y"). Returned by `messages_for_conversation` for rehydration and
/// the chat_history fallback path.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct ConvRecord {
    id: Uuid,
    channel: String,
    user_id: String,
    thread_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    /// User-pinned threads sort to the top of the sidebar. Pin state
    /// is a manual toggle from the right-click menu; pinned_at orders
    /// the pinned section (most-recently-pinned first).
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    pinned_at: Option<DateTime<Utc>>,
    /// Optional grouping: a Venture is a collapsible sidebar section.
    /// `None` means the thread is "loose" — shown below the venture
    /// groups in its own unsectioned block.
    #[serde(default)]
    venture_id: Option<Uuid>,
}

/// Persisted Venture: a named, collapsible sidebar group that bundles
/// related threads. One thread belongs to at most one Venture (or
/// none); ventures persist their `collapsed` state so the visual
/// shape survives restarts.
#[derive(Serialize, Deserialize, Clone)]
pub struct VentureRecord {
    pub id: Uuid,
    pub user_id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Serialize, Deserialize)]
struct MsgRecord {
    id: Uuid,
    conversation_id: Uuid,
    role: String,
    content: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct LlmCallStored {
    id: Uuid,
    job_id: Option<Uuid>,
    conversation_id: Option<Uuid>,
    provider: String,
    model: String,
    input_tokens: u32,
    output_tokens: u32,
    cost: Decimal,
    purpose: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Clone)]
struct EstimationStored {
    id: Uuid,
    job_id: Uuid,
    category: String,
    tool_names: Vec<String>,
    estimated_cost: Decimal,
    estimated_time_secs: i32,
    estimated_value: Decimal,
    actual_cost: Option<Decimal>,
    actual_time_secs: Option<i32>,
    actual_value: Option<Decimal>,
    created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ToolFailureStored {
    tool_name: String,
    error_message: String,
    error_count: i32,
    first_failure: DateTime<Utc>,
    last_failure: DateTime<Utc>,
    last_build_result: Option<serde_json::Value>,
    repair_attempts: i32,
    repaired_at: Option<DateTime<Utc>>,
}

/// Fjall-backed replacement for the PostgreSQL history `Store`.
pub struct FjallHistoryStore {
    keyspace: Keyspace,
    conversations: PartitionHandle,
    messages: PartitionHandle,
    jobs: PartitionHandle,
    actions: PartitionHandle,
    llm_calls: PartitionHandle,
    estimations: PartitionHandle,
    tool_failures: PartitionHandle,
    /// Named, collapsible thread groups (the "Venture" concept in the
    /// sidebar UX). One row per venture; the per-thread venture_id
    /// lives on the ConvRecord side so we don't have to maintain a
    /// reverse index — a thread's venture is wherever its own row says.
    ventures: PartitionHandle,
}

fn job_action_key(job_id: Uuid, seq: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(20);
    k.extend_from_slice(job_id.as_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

impl FjallHistoryStore {
    /// Open (or create) the history store at `path`.
    pub fn open(path: &str) -> Result<Self, DatabaseError> {
        let keyspace = Config::new(path).open().map_err(|e| db_err("open keyspace", e))?;
        let p = |name: &str| {
            keyspace
                .open_partition(name, PartitionCreateOptions::default())
                .map_err(|e| db_err("open partition", e))
        };
        Ok(Self {
            conversations: p("conversations")?,
            messages: p("messages")?,
            jobs: p("jobs")?,
            actions: p("actions")?,
            llm_calls: p("llm_calls")?,
            estimations: p("estimations")?,
            tool_failures: p("tool_failures")?,
            ventures: p("ventures")?,
            keyspace,
        })
    }

    fn persist(&self) -> Result<(), DatabaseError> {
        self.keyspace
            .persist(PersistMode::Buffer)
            .map_err(|e| db_err("persist", e))
    }

    fn put_json<T: Serialize>(
        part: &PartitionHandle,
        key: impl Into<Vec<u8>>,
        value: &T,
    ) -> Result<(), DatabaseError> {
        let bytes = serde_json::to_vec(value).map_err(|e| db_err("encode", e))?;
        part.insert(key.into(), bytes).map_err(|e| db_err("put", e))
    }

    fn get_json<T: for<'de> Deserialize<'de>>(
        part: &PartitionHandle,
        key: &[u8],
    ) -> Result<Option<T>, DatabaseError> {
        match part.get(key).map_err(|e| db_err("get", e))? {
            Some(b) => Ok(Some(serde_json::from_slice(&b).map_err(|e| db_err("decode", e))?)),
            None => Ok(None),
        }
    }

    // ==================== Conversations ====================

    pub async fn create_conversation(
        &self,
        channel: &str,
        user_id: &str,
        thread_id: Option<&str>,
    ) -> Result<Uuid, DatabaseError> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let rec = ConvRecord {
            id,
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            thread_id: thread_id.map(|s| s.to_string()),
            title: None,
            created_at: now,
            last_activity: now,
            pinned: false,
            pinned_at: None,
            venture_id: None,
        };
        Self::put_json(&self.conversations, id.as_bytes().to_vec(), &rec)?;
        self.persist()?;
        Ok(id)
    }

    pub async fn touch_conversation(&self, id: Uuid) -> Result<(), DatabaseError> {
        if let Some(mut rec) = Self::get_json::<ConvRecord>(&self.conversations, id.as_bytes())? {
            rec.last_activity = Utc::now();
            Self::put_json(&self.conversations, id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    /// Convenience for the chat path: ensure a Conversation row exists
    /// for the given thread, keyed by the thread UUID directly (so the
    /// thread_id IS the conversation_id from this layer's POV — no
    /// secondary mapping needed). Idempotent: subsequent calls just
    /// bump `last_activity`.
    pub async fn upsert_conversation_for_thread(
        &self,
        thread_id: Uuid,
        channel: &str,
        user_id: &str,
    ) -> Result<(), DatabaseError> {
        let now = Utc::now();
        let rec = match Self::get_json::<ConvRecord>(
            &self.conversations,
            thread_id.as_bytes(),
        )? {
            Some(mut existing) => {
                existing.last_activity = now;
                existing
            }
            None => ConvRecord {
                id: thread_id,
                channel: channel.to_string(),
                user_id: user_id.to_string(),
                thread_id: Some(thread_id.to_string()),
                title: None,
                created_at: now,
                last_activity: now,
                pinned: false,
                pinned_at: None,
                venture_id: None,
            },
        };
        Self::put_json(&self.conversations, thread_id.as_bytes().to_vec(), &rec)?;
        self.persist()?;
        Ok(())
    }

    /// List every conversation for `user_id`, sorted newest-first by
    /// `last_activity`. Used by session rehydration on Agent boot to
    /// rebuild the in-memory thread map so the left-sidebar threads
    /// list survives restarts.
    pub async fn list_conversations_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<ConversationSummary>, DatabaseError> {
        let mut out: Vec<ConversationSummary> = Vec::new();
        for kv in self.conversations.iter() {
            let (_, v) = kv.map_err(|e| db_err("scan conversations", e))?;
            if let Ok(rec) = serde_json::from_slice::<ConvRecord>(&v) {
                if rec.user_id == user_id {
                    out.push(ConversationSummary {
                        id: rec.id,
                        thread_id: rec
                            .thread_id
                            .and_then(|s| Uuid::parse_str(&s).ok()),
                        channel: rec.channel,
                        title: rec.title,
                        created_at: rec.created_at,
                        last_activity: rec.last_activity,
                        pinned: rec.pinned,
                        pinned_at: rec.pinned_at,
                        venture_id: rec.venture_id,
                    });
                }
            }
        }
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        Ok(out)
    }

    /// Set/clear the user-assigned title for a thread's persisted
    /// conversation. No-op if the conversation row doesn't exist yet.
    pub async fn set_conversation_title(
        &self,
        thread_id: Uuid,
        title: Option<String>,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<ConvRecord>(&self.conversations, thread_id.as_bytes())?
        {
            rec.title = title;
            rec.last_activity = Utc::now();
            Self::put_json(&self.conversations, thread_id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    /// Toggle the user-pin flag for a thread. Pinning stamps
    /// `pinned_at = now()` so the pinned section can sort
    /// most-recently-pinned first; unpinning clears it.
    pub async fn set_conversation_pinned(
        &self,
        thread_id: Uuid,
        pinned: bool,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<ConvRecord>(&self.conversations, thread_id.as_bytes())?
        {
            rec.pinned = pinned;
            rec.pinned_at = pinned.then(Utc::now);
            Self::put_json(&self.conversations, thread_id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    /// Assign or clear a thread's venture. Passing `None` removes the
    /// thread from whichever venture it was in (drops it to the loose
    /// section). The venture row itself is untouched.
    pub async fn set_conversation_venture(
        &self,
        thread_id: Uuid,
        venture_id: Option<Uuid>,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<ConvRecord>(&self.conversations, thread_id.as_bytes())?
        {
            rec.venture_id = venture_id;
            Self::put_json(&self.conversations, thread_id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    // ==================== Ventures ====================

    /// Create a new venture for the given user. Returns the assigned
    /// id; the caller is expected to push it to clients via the next
    /// `list_threads` poll.
    pub async fn create_venture(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Uuid, DatabaseError> {
        let id = Uuid::new_v4();
        let rec = VentureRecord {
            id,
            user_id: user_id.to_string(),
            name: name.to_string(),
            created_at: Utc::now(),
            collapsed: false,
        };
        Self::put_json(&self.ventures, id.as_bytes().to_vec(), &rec)?;
        self.persist()?;
        Ok(id)
    }

    /// List all ventures for a user, oldest-first (sidebar shows them
    /// in creation order so the layout doesn't shuffle as the user
    /// renames or expands/collapses).
    pub async fn list_ventures_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<VentureSummary>, DatabaseError> {
        let mut out: Vec<VentureSummary> = Vec::new();
        for kv in self.ventures.iter() {
            let (_, v) = kv.map_err(|e| db_err("scan ventures", e))?;
            if let Ok(rec) = serde_json::from_slice::<VentureRecord>(&v) {
                if rec.user_id == user_id {
                    out.push(VentureSummary {
                        id: rec.id,
                        name: rec.name,
                        created_at: rec.created_at,
                        collapsed: rec.collapsed,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    /// Rename a venture. No-op if the venture doesn't exist.
    pub async fn rename_venture(
        &self,
        venture_id: Uuid,
        name: &str,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<VentureRecord>(&self.ventures, venture_id.as_bytes())?
        {
            rec.name = name.to_string();
            Self::put_json(&self.ventures, venture_id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    /// Toggle the venture's collapsed-state flag. Persistent so the
    /// sidebar shape survives jarvis-desktop restarts.
    pub async fn set_venture_collapsed(
        &self,
        venture_id: Uuid,
        collapsed: bool,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<VentureRecord>(&self.ventures, venture_id.as_bytes())?
        {
            rec.collapsed = collapsed;
            Self::put_json(&self.ventures, venture_id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    /// Delete a venture. All threads previously in it have their
    /// `venture_id` cleared (they fall back to the loose section);
    /// the threads themselves stay intact.
    pub async fn delete_venture(
        &self,
        venture_id: Uuid,
    ) -> Result<(), DatabaseError> {
        // Unassign every conversation that pointed at this venture.
        let mut to_update: Vec<(Vec<u8>, ConvRecord)> = Vec::new();
        for kv in self.conversations.iter() {
            let (k, v) = kv.map_err(|e| db_err("scan conversations for venture delete", e))?;
            if let Ok(rec) = serde_json::from_slice::<ConvRecord>(&v) {
                if rec.venture_id == Some(venture_id) {
                    let mut updated = rec;
                    updated.venture_id = None;
                    to_update.push((k.to_vec(), updated));
                }
            }
        }
        for (k, rec) in to_update {
            Self::put_json(&self.conversations, k, &rec)?;
        }
        // Then the venture row itself.
        self.ventures
            .remove(venture_id.as_bytes())
            .map_err(|e| db_err("remove venture", e))?;
        self.persist()?;
        Ok(())
    }

    /// Remove a conversation and ALL of its messages from the store.
    /// Used by the sidebar's right-click → Delete flow. Idempotent: a
    /// missing thread returns Ok(()).
    pub async fn delete_conversation(
        &self,
        thread_id: Uuid,
    ) -> Result<(), DatabaseError> {
        // Drop messages first (prefix-scan + collect keys, then remove).
        let prefix = thread_id.as_bytes().to_vec();
        let mut to_drop: Vec<Vec<u8>> = Vec::new();
        for kv in self.messages.prefix(&prefix) {
            let (k, _) = kv.map_err(|e| db_err("scan messages for delete", e))?;
            to_drop.push(k.to_vec());
        }
        for k in to_drop {
            self.messages
                .remove(&k)
                .map_err(|e| db_err("remove message", e))?;
        }
        // Then the conversation row itself.
        self.conversations
            .remove(thread_id.as_bytes())
            .map_err(|e| db_err("remove conversation", e))?;
        self.persist()?;
        Ok(())
    }

    /// Read every message for a conversation, sorted by creation time.
    /// Used by both rehydration and the chat_history_handler fallback
    /// path.
    pub async fn messages_for_conversation(
        &self,
        conversation_id: Uuid,
    ) -> Result<Vec<ConversationMessage>, DatabaseError> {
        let prefix = conversation_id.as_bytes().to_vec();
        let mut by_ts: std::collections::BTreeMap<
            (DateTime<Utc>, Uuid),
            ConversationMessage,
        > = std::collections::BTreeMap::new();
        for kv in self.messages.prefix(&prefix) {
            let (_, v) = kv.map_err(|e| db_err("scan messages", e))?;
            if let Ok(rec) = serde_json::from_slice::<MsgRecord>(&v) {
                by_ts.insert(
                    (rec.created_at, rec.id),
                    ConversationMessage {
                        role: rec.role,
                        content: rec.content,
                        created_at: rec.created_at,
                    },
                );
            }
        }
        Ok(by_ts.into_values().collect())
    }

    pub async fn add_conversation_message(
        &self,
        conversation_id: Uuid,
        role: &str,
        content: &str,
    ) -> Result<Uuid, DatabaseError> {
        let id = Uuid::new_v4();
        let rec = MsgRecord {
            id,
            conversation_id,
            role: role.to_string(),
            content: content.to_string(),
            created_at: Utc::now(),
        };
        let mut key = conversation_id.as_bytes().to_vec();
        key.extend_from_slice(id.as_bytes());
        Self::put_json(&self.messages, key, &rec)?;
        // touch_conversation only persists when the parent conv record
        // exists — without this fallback persist, a message written
        // before its conv row is upserted (e.g. the agent-loop spawn
        // race where the user-msg + upsert task runs concurrently with
        // an assistant-msg task) would sit in the memtable and be lost
        // on the next gateway shutdown. Persist unconditionally so the
        // write durably hits disk regardless of spawn ordering.
        self.touch_conversation(conversation_id).await?;
        self.persist()?;
        Ok(id)
    }

    // ==================== Jobs ====================

    pub async fn save_job(&self, ctx: &JobContext) -> Result<(), DatabaseError> {
        Self::put_json(&self.jobs, ctx.job_id.as_bytes().to_vec(), ctx)?;
        self.persist()
    }

    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobContext>, DatabaseError> {
        Self::get_json(&self.jobs, id.as_bytes())
    }

    pub async fn update_job_status(
        &self,
        id: Uuid,
        status: JobState,
        _failure_reason: Option<&str>,
    ) -> Result<(), DatabaseError> {
        if let Some(mut ctx) = self.get_job(id).await? {
            ctx.state = status;
            self.save_job(&ctx).await?;
        }
        Ok(())
    }

    pub async fn mark_job_stuck(&self, id: Uuid) -> Result<(), DatabaseError> {
        if let Some(mut ctx) = self.get_job(id).await? {
            ctx.state = JobState::Stuck;
            self.save_job(&ctx).await?;
        }
        Ok(())
    }

    pub async fn get_stuck_jobs(&self) -> Result<Vec<Uuid>, DatabaseError> {
        let mut ids = Vec::new();
        for kv in self.jobs.iter() {
            let (_k, v) = kv.map_err(|e| db_err("scan jobs", e))?;
            let ctx: JobContext = serde_json::from_slice(&v).map_err(|e| db_err("decode job", e))?;
            if ctx.state == JobState::Stuck {
                ids.push(ctx.job_id);
            }
        }
        Ok(ids)
    }

    // ==================== Actions ====================

    pub async fn save_action(
        &self,
        job_id: Uuid,
        action: &ActionRecord,
    ) -> Result<(), DatabaseError> {
        Self::put_json(&self.actions, job_action_key(job_id, action.sequence), action)?;
        self.persist()
    }

    pub async fn get_job_actions(&self, job_id: Uuid) -> Result<Vec<ActionRecord>, DatabaseError> {
        let mut actions = Vec::new();
        for kv in self.actions.prefix(job_id.as_bytes().to_vec()) {
            let (_k, v) = kv.map_err(|e| db_err("scan actions", e))?;
            actions.push(serde_json::from_slice(&v).map_err(|e| db_err("decode action", e))?);
        }
        // prefix iterates in key order = sequence order (be u32).
        Ok(actions)
    }

    // ==================== LLM Calls ====================

    pub async fn record_llm_call(&self, record: &LlmCallRecord<'_>) -> Result<Uuid, DatabaseError> {
        let id = Uuid::new_v4();
        let stored = LlmCallStored {
            id,
            job_id: record.job_id,
            conversation_id: record.conversation_id,
            provider: record.provider.to_string(),
            model: record.model.to_string(),
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cost: record.cost,
            purpose: record.purpose.map(|s| s.to_string()),
            created_at: Utc::now(),
        };
        Self::put_json(&self.llm_calls, id.as_bytes().to_vec(), &stored)?;
        self.persist()?;
        Ok(id)
    }

    // ==================== Estimation Snapshots ====================

    pub async fn save_estimation_snapshot(
        &self,
        job_id: Uuid,
        category: &str,
        tool_names: &[String],
        estimated_cost: Decimal,
        estimated_time_secs: i32,
        estimated_value: Decimal,
    ) -> Result<Uuid, DatabaseError> {
        let id = Uuid::new_v4();
        let rec = EstimationStored {
            id,
            job_id,
            category: category.to_string(),
            tool_names: tool_names.to_vec(),
            estimated_cost,
            estimated_time_secs,
            estimated_value,
            actual_cost: None,
            actual_time_secs: None,
            actual_value: None,
            created_at: Utc::now(),
        };
        Self::put_json(&self.estimations, id.as_bytes().to_vec(), &rec)?;
        self.persist()?;
        Ok(id)
    }

    pub async fn update_estimation_actuals(
        &self,
        id: Uuid,
        actual_cost: Decimal,
        actual_time_secs: i32,
        actual_value: Option<Decimal>,
    ) -> Result<(), DatabaseError> {
        if let Some(mut rec) = Self::get_json::<EstimationStored>(&self.estimations, id.as_bytes())? {
            rec.actual_cost = Some(actual_cost);
            rec.actual_time_secs = Some(actual_time_secs);
            rec.actual_value = actual_value;
            Self::put_json(&self.estimations, id.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    fn all_estimations(&self) -> Result<Vec<EstimationStored>, DatabaseError> {
        let mut out = Vec::new();
        for kv in self.estimations.iter() {
            let (_k, v) = kv.map_err(|e| db_err("scan estimations", e))?;
            out.push(serde_json::from_slice(&v).map_err(|e| db_err("decode estimation", e))?);
        }
        Ok(out)
    }

    // ==================== Tool Failures ====================

    pub async fn record_tool_failure(
        &self,
        tool_name: &str,
        error_message: &str,
    ) -> Result<(), DatabaseError> {
        let now = Utc::now();
        let rec = match Self::get_json::<ToolFailureStored>(
            &self.tool_failures,
            tool_name.as_bytes(),
        )? {
            Some(mut existing) => {
                existing.error_message = error_message.to_string();
                existing.error_count += 1;
                existing.last_failure = now;
                existing
            }
            None => ToolFailureStored {
                tool_name: tool_name.to_string(),
                error_message: error_message.to_string(),
                error_count: 1,
                first_failure: now,
                last_failure: now,
                last_build_result: None,
                repair_attempts: 0,
                repaired_at: None,
            },
        };
        Self::put_json(&self.tool_failures, tool_name.as_bytes().to_vec(), &rec)?;
        self.persist()
    }

    pub async fn get_broken_tools(&self, threshold: i32) -> Result<Vec<BrokenTool>, DatabaseError> {
        let mut out = Vec::new();
        for kv in self.tool_failures.iter() {
            let (_k, v) = kv.map_err(|e| db_err("scan tool_failures", e))?;
            let rec: ToolFailureStored =
                serde_json::from_slice(&v).map_err(|e| db_err("decode failure", e))?;
            if rec.error_count >= threshold && rec.repaired_at.is_none() {
                out.push(BrokenTool {
                    name: rec.tool_name,
                    last_error: Some(rec.error_message),
                    failure_count: rec.error_count as u32,
                    first_failure: rec.first_failure,
                    last_failure: rec.last_failure,
                    last_build_result: rec.last_build_result,
                    repair_attempts: rec.repair_attempts as u32,
                });
            }
        }
        out.sort_by(|a, b| b.failure_count.cmp(&a.failure_count));
        Ok(out)
    }

    pub async fn mark_tool_repaired(&self, tool_name: &str) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<ToolFailureStored>(&self.tool_failures, tool_name.as_bytes())?
        {
            rec.repaired_at = Some(Utc::now());
            rec.error_count = 0;
            Self::put_json(&self.tool_failures, tool_name.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    pub async fn increment_repair_attempts(&self, tool_name: &str) -> Result<(), DatabaseError> {
        if let Some(mut rec) =
            Self::get_json::<ToolFailureStored>(&self.tool_failures, tool_name.as_bytes())?
        {
            rec.repair_attempts += 1;
            Self::put_json(&self.tool_failures, tool_name.as_bytes().to_vec(), &rec)?;
            self.persist()?;
        }
        Ok(())
    }

    // ==================== Analytics ====================

    pub async fn get_job_stats(&self) -> Result<JobStats, DatabaseError> {
        let mut total = 0u64;
        let mut completed = 0u64;
        let mut failed = 0u64;
        let mut duration_sum = 0.0f64;
        let mut duration_n = 0u64;
        let mut cost_sum = Decimal::ZERO;
        for kv in self.jobs.iter() {
            let (_k, v) = kv.map_err(|e| db_err("scan jobs", e))?;
            let ctx: JobContext = serde_json::from_slice(&v).map_err(|e| db_err("decode job", e))?;
            total += 1;
            if ctx.state == JobState::Accepted {
                completed += 1;
            }
            if ctx.state == JobState::Failed {
                failed += 1;
            }
            if let (Some(start), Some(end)) = (ctx.started_at, ctx.completed_at) {
                duration_sum += (end - start).num_seconds() as f64;
                duration_n += 1;
            }
            cost_sum += ctx.actual_cost;
        }
        Ok(JobStats {
            total_jobs: total,
            completed_jobs: completed,
            failed_jobs: failed,
            success_rate: if total > 0 { completed as f64 / total as f64 } else { 0.0 },
            avg_duration_secs: if duration_n > 0 { duration_sum / duration_n as f64 } else { 0.0 },
            avg_cost: if total > 0 { cost_sum / Decimal::from(total) } else { Decimal::ZERO },
            total_cost: cost_sum,
        })
    }

    pub async fn get_tool_stats(&self) -> Result<Vec<ToolStats>, DatabaseError> {
        use std::collections::HashMap;
        struct Acc {
            total: u64,
            ok: u64,
            failed: u64,
            dur_ms: f64,
            cost: Decimal,
        }
        let mut by_tool: HashMap<String, Acc> = HashMap::new();
        for kv in self.actions.iter() {
            let (_k, v) = kv.map_err(|e| db_err("scan actions", e))?;
            let a: ActionRecord = serde_json::from_slice(&v).map_err(|e| db_err("decode action", e))?;
            let e = by_tool.entry(a.tool_name.clone()).or_insert(Acc {
                total: 0,
                ok: 0,
                failed: 0,
                dur_ms: 0.0,
                cost: Decimal::ZERO,
            });
            e.total += 1;
            if a.success {
                e.ok += 1;
            } else {
                e.failed += 1;
            }
            e.dur_ms += a.duration.as_millis() as f64;
            e.cost += a.cost.unwrap_or(Decimal::ZERO);
        }
        let mut stats: Vec<ToolStats> = by_tool
            .into_iter()
            .map(|(tool_name, a)| ToolStats {
                tool_name,
                total_calls: a.total,
                successful_calls: a.ok,
                failed_calls: a.failed,
                success_rate: if a.total > 0 { a.ok as f64 / a.total as f64 } else { 0.0 },
                avg_duration_ms: if a.total > 0 { a.dur_ms / a.total as f64 } else { 0.0 },
                total_cost: a.cost,
            })
            .collect();
        stats.sort_by(|a, b| b.total_calls.cmp(&a.total_calls));
        Ok(stats)
    }

    pub async fn get_estimation_accuracy(
        &self,
        category: Option<&str>,
    ) -> Result<EstimationAccuracy, DatabaseError> {
        let mut cost_err = 0.0f64;
        let mut time_err = 0.0f64;
        let mut n = 0u64;
        for rec in self.all_estimations()? {
            let Some(actual_cost) = rec.actual_cost else { continue };
            if let Some(cat) = category {
                if rec.category != cat {
                    continue;
                }
            }
            if !rec.estimated_cost.is_zero() {
                let diff = (actual_cost - rec.estimated_cost).abs() / rec.estimated_cost;
                cost_err += diff.to_string().parse::<f64>().unwrap_or(0.0);
            }
            if rec.estimated_time_secs != 0 {
                if let Some(actual_t) = rec.actual_time_secs {
                    time_err += (actual_t - rec.estimated_time_secs).unsigned_abs() as f64
                        / rec.estimated_time_secs as f64;
                }
            }
            n += 1;
        }
        Ok(EstimationAccuracy {
            cost_error_rate: if n > 0 { cost_err / n as f64 } else { 0.0 },
            time_error_rate: if n > 0 { time_err / n as f64 } else { 0.0 },
            sample_count: n,
        })
    }

    pub async fn get_category_history(
        &self,
        category: &str,
        limit: i64,
    ) -> Result<Vec<CategoryHistoryEntry>, DatabaseError> {
        let mut entries: Vec<EstimationStored> = self
            .all_estimations()?
            .into_iter()
            .filter(|r| r.category == category && r.actual_cost.is_some())
            .collect();
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        entries.truncate(limit.max(0) as usize);
        Ok(entries
            .into_iter()
            .map(|r| CategoryHistoryEntry {
                tool_names: r.tool_names,
                estimated_cost: r.estimated_cost,
                actual_cost: r.actual_cost,
                estimated_time_secs: r.estimated_time_secs,
                actual_time_secs: r.actual_time_secs,
                created_at: r.created_at,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn temp_store() -> (tempfile::TempDir, FjallHistoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FjallHistoryStore::open(&dir.path().join("hist").to_string_lossy()).unwrap();
        (dir, s)
    }

    fn action(seq: u32, tool: &str) -> ActionRecord {
        ActionRecord {
            id: Uuid::new_v4(),
            sequence: seq,
            tool_name: tool.to_string(),
            input: serde_json::Value::Null,
            output_raw: None,
            output_sanitized: None,
            sanitization_warnings: Vec::new(),
            cost: None,
            duration: std::time::Duration::from_millis(5),
            success: true,
            error: None,
            executed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn jobs_save_get_status_stuck() {
        let (_d, s) = temp_store();
        let ctx = JobContext::new("title", "desc");
        s.save_job(&ctx).await.unwrap();
        assert_eq!(s.get_job(ctx.job_id).await.unwrap().unwrap().title, "title");

        s.update_job_status(ctx.job_id, JobState::InProgress, None).await.unwrap();
        assert_eq!(s.get_job(ctx.job_id).await.unwrap().unwrap().state, JobState::InProgress);

        s.mark_job_stuck(ctx.job_id).await.unwrap();
        assert_eq!(s.get_stuck_jobs().await.unwrap(), vec![ctx.job_id]);
    }

    #[tokio::test]
    async fn actions_ordered_by_sequence() {
        let (_d, s) = temp_store();
        let job = Uuid::new_v4();
        for seq in [2u32, 0, 1] {
            s.save_action(job, &action(seq, &format!("tool{seq}"))).await.unwrap();
        }
        let got = s.get_job_actions(job).await.unwrap();
        let seqs: Vec<u32> = got.iter().map(|a| a.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn tool_stats_aggregate() {
        let (_d, s) = temp_store();
        let job = Uuid::new_v4();
        s.save_action(job, &action(0, "alpha")).await.unwrap();
        s.save_action(job, &action(1, "alpha")).await.unwrap();
        let stats = s.get_tool_stats().await.unwrap();
        let alpha = stats.iter().find(|t| t.tool_name == "alpha").unwrap();
        assert_eq!(alpha.total_calls, 2);
    }

    #[tokio::test]
    async fn tool_failures_lifecycle() {
        let (_d, s) = temp_store();
        s.record_tool_failure("t", "boom").await.unwrap();
        s.record_tool_failure("t", "boom again").await.unwrap();
        let broken = s.get_broken_tools(2).await.unwrap();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].failure_count, 2);

        s.mark_tool_repaired("t").await.unwrap();
        assert!(s.get_broken_tools(1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn estimation_and_job_stats() {
        let (_d, s) = temp_store();
        let job = Uuid::new_v4();
        let est = s
            .save_estimation_snapshot(job, "cat", &["x".into()], dec!(10), 100, dec!(50))
            .await
            .unwrap();
        s.update_estimation_actuals(est, dec!(12), 110, Some(dec!(60))).await.unwrap();

        let acc = s.get_estimation_accuracy(Some("cat")).await.unwrap();
        assert_eq!(acc.sample_count, 1);

        let hist = s.get_category_history("cat", 10).await.unwrap();
        assert_eq!(hist.len(), 1);

        let stats = s.get_job_stats().await.unwrap();
        assert_eq!(stats.total_jobs, 0); // no jobs saved
    }
}
