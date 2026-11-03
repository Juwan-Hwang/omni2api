//! PostgreSQL implementation of all repository traits (except ReplyRegistry).
//!
//! Phase 5A: Persistent storage backed by SQLx + PostgreSQL.
//! Phase 7: Agent capability + context persistence.
//!
//! `PostgresStore` implements `IdentityRepository`, `ConversationRepository`,
//! `MessageRepository`, `CapabilityRepository`, and `ContextRepository`.
//! The `ReplyRegistry` trait is implemented by the in-memory `InMemoryReplyRegistry`
//! — because reply channels carry live `mpsc::Sender` objects that are inherently
//! ephemeral and cannot be persisted to a database.
//!
//! # Architecture
//!
//! ```text
//!   AppState
//!     ├── identity:    Arc<dyn IdentityRepository>       ← PostgresStore
//!     ├── conversation: Arc<dyn ConversationRepository>  ← PostgresStore
//!     ├── message:     Arc<dyn MessageRepository>        ← PostgresStore
//!     ├── capability:  Arc<dyn CapabilityRepository>     ← PostgresStore
//!     ├── context:     Arc<dyn ContextRepository>        ← PostgresStore
//!     └── reply:       Arc<dyn ReplyRegistry>           ← InMemoryReplyRegistry
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use uuid::Uuid;

use sqlx::postgres::PgPool;
use sqlx::Executor;
use sqlx::Row;

use crate::model::agent_capability::AgentCapability;
use crate::model::agent_context::AgentContext;
use crate::model::content::ContentBlock;
use crate::model::conversation::{
    Conversation, ConversationParticipant, ConversationStatus, ConversationType, ParticipantRole,
};
use crate::model::identity::{
    AgentIdentity, ApiKey, ClientType, HumanIdentity, HumanStatus, ParticipantRef, ParticipantType,
};
use crate::model::message::{Message, MessageStatus};
use crate::state::AuthPrincipal;
use crate::storage::traits::{
    CapabilityRepository, ContextRepository, ConversationRepository, IdentityRepository,
    MessageRepository, ReplyRegistry,
};

// ════════════════════════════════════════════════════════════
//  PostgresStore
// ════════════════════════════════════════════════════════════

/// PostgreSQL-backed store implementing identity, conversation, and message
/// repository traits.
///
/// Cloning is cheap (inner `Arc<PgPool>`).
#[derive(Clone)]
pub struct PostgresStore {
    pool: Arc<PgPool>,
}

impl PostgresStore {
    /// Connect to a PostgreSQL database and return a `PostgresStore`.
    ///
    /// The connection string should be a standard PostgreSQL URL:
    /// `postgresql://user:password@host:5432/dbname`
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPool::connect(database_url).await?;
        Ok(Self {
            pool: Arc::new(pool),
        })
    }

    /// Create a `PostgresStore` from an existing `PgPool` (useful in tests).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool: Arc::new(pool),
        }
    }

    /// Run all migration SQL files to create tables and seed data.
    ///
    /// Executes migrations in order: 001_init -> 002_capabilities ->
    /// 003_api_key_hashing -> 004_conversation_trash -> 005_tool_call_lookup.
    /// This is a simple `execute` of SQL file contents
    /// — not a full migration framework, but sufficient for Phase 5-7.
    ///
    /// All files are written to be re-runnable, so this is safe to
    /// call on every boot.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        let sql1 = include_str!("../../migrations/001_init.sql");
        self.pool.execute(sql1).await?;
        let sql2 = include_str!("../../migrations/002_capabilities.sql");
        self.pool.execute(sql2).await?;
        // 003 backfills real SHA-256 digests and drops the plaintext
        // `raw_key` column. It must run last — it reads `raw_key` in order
        // to derive the digests it replaces it with.
        let sql3 = include_str!("../../migrations/003_api_key_hashing.sql");
        self.pool.execute(sql3).await?;
        let sql4 = include_str!("../../migrations/004_conversation_trash.sql");
        self.pool.execute(sql4).await?;
        let sql5 = include_str!("../../migrations/005_tool_call_lookup.sql");
        self.pool.execute(sql5).await?;
        let sql6 = include_str!("../../migrations/006_key_scope.sql");
        self.pool.execute(sql6).await?;
        // 001 re-seeds the public demo key on every boot. Outside an
        // explicit demo it must not authenticate anything.
        if std::env::var("OMNI2API_DEMO").ok().as_deref() != Some("1") {
            self.pool
                .execute("UPDATE api_keys SET scope = 'revoked' WHERE id = 'key_demo_001'")
                .await?;
        } else {
            tracing::warn!("OMNI2API_DEMO=1 — the public demo key is live");
        }
        Ok(())
    }

    /// Expose the pool (for tests or advanced usage).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ── IdentityRepository ──

#[async_trait]
impl IdentityRepository for PostgresStore {
    /// Resolve an API key to its principal, looked up **by SHA-256 digest**.
    ///
    /// The digest is computed from the inbound key and matched against
    /// `api_keys.key_hash`. The plaintext key never touches a query, and
    /// (after migration `003`) no plaintext column exists to leak.
    ///
    /// The returned `ApiKey.key` carries the raw key for the duration of
    /// this request only — see `ApiKey::persisted` for the form that is
    /// allowed to reach storage.
    async fn resolve_principal(&self, raw_key: &str) -> Option<AuthPrincipal> {
        let key_hash = api_key_hash(raw_key);

        let key_row =
            sqlx::query("SELECT id, agent_id, human_id, scope FROM api_keys WHERE key_hash = $1")
                .bind(&key_hash)
                .fetch_optional(&*self.pool)
                .await
                .ok()??;

        let key_id: String = key_row.try_get("id").ok()?;
        let agent_id: String = key_row.try_get("agent_id").ok()?;
        let human_id: String = key_row.try_get("human_id").ok()?;
        let scope: String = key_row
            .try_get("scope")
            .unwrap_or_else(|_| "both".to_string());

        let agent = self.get_agent_inner(&agent_id).await?;
        let human = self.get_human_inner(&human_id).await?;

        Some(AuthPrincipal {
            agent,
            human,
            api_key: ApiKey {
                id: key_id,
                key: raw_key.to_string(),
                key_hash: Some(key_hash),
                agent_id,
                human_id,
                scope: crate::model::identity::KeyScope::parse(&scope),
            },
        })
    }

    async fn rename_agent(
        &self,
        agent_id: &str,
        human_id: &str,
        api_key_id: &str,
        name: &str,
    ) -> Option<AgentIdentity> {
        // The presented key must be the one this agent was issued under.
        // Matching on `(agent, human)` alone is too loose: two agents can
        // share a human, and then either key could rename the other.
        let result = sqlx::query(
            "UPDATE agent_identities AS a \
             SET name = $1, updated_at = now() \
             FROM api_keys k \
             WHERE a.id = $2 AND k.id = $3 AND k.agent_id = a.id \
               AND k.human_id = $4",
        )
        .bind(name)
        .bind(agent_id)
        .bind(api_key_id)
        .bind(human_id)
        .execute(&*self.pool)
        .await;

        let affected = match &result {
            Ok(r) => r.rows_affected(),
            Err(_) => 0,
        };
        tracing::info!(
            "rename_agent: agent={agent_id} human={human_id} name={name} \
             rows_affected={affected} err={:?}",
            result.as_ref().err()
        );

        match result {
            Ok(r) if r.rows_affected() > 0 => {
                let agent = self.get_agent(agent_id).await;
                tracing::info!("rename_agent: read back = {:?}", agent.is_some());
                agent
            }
            _ => None,
        }
    }

    async fn insert_human(&self, human: HumanIdentity) {
        let _ = sqlx::query(
            r#"
            INSERT INTO identities (id, display_name, avatar_url, status, capabilities, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, 'online', $4, $5, $6, $7)
            ON CONFLICT (id) DO UPDATE SET
                display_name = EXCLUDED.display_name,
                avatar_url   = EXCLUDED.avatar_url,
                capabilities = EXCLUDED.capabilities,
                metadata     = EXCLUDED.metadata,
                updated_at   = EXCLUDED.updated_at
            "#,
        )
        .bind(&human.id)
        .bind(&human.display_name)
        .bind(&human.avatar_url)
        .bind(serde_json::to_value(&human.capabilities).unwrap_or_default())
        .bind(serde_json::to_value(&human.metadata).unwrap_or_default())
        .bind(human.created_at)
        .bind(human.updated_at)
        .execute(&*self.pool)
        .await;
    }

    async fn insert_agent(&self, agent: AgentIdentity) {
        let client_type_str = match agent.client_type {
            ClientType::Codex => "codex",
            ClientType::ClaudeCode => "claude_code",
            ClientType::Trae => "trae",
            ClientType::Custom => "custom",
        };

        let _ = sqlx::query(
            r#"
            INSERT INTO agent_identities (id, name, client_type, api_key_id, capabilities, metadata, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE SET
                name         = EXCLUDED.name,
                client_type  = EXCLUDED.client_type,
                capabilities = EXCLUDED.capabilities,
                metadata     = EXCLUDED.metadata,
                updated_at   = EXCLUDED.updated_at
            "#,
        )
        .bind(&agent.id)
        .bind(&agent.name)
        .bind(client_type_str)
        .bind(&agent.api_key_id)
        .bind(serde_json::to_value(&agent.capabilities).unwrap_or_default())
        .bind(serde_json::to_value(&agent.metadata).unwrap_or_default())
        .bind(agent.created_at)
        .bind(agent.updated_at)
        .execute(&*self.pool)
        .await;
    }

    /// Store an API key — **digest only.**
    ///
    /// The plaintext is stripped via `ApiKey::persisted` before binding, and
    /// the `raw_key` column is not referenced at all. Migration `003` drops
    /// that column outright.
    ///
    /// If the caller hands over a record whose `key_hash` is absent (a legacy
    /// in-memory record), the digest is derived from the raw key here, so a
    /// plaintext value still never reaches the database.
    async fn insert_api_key(&self, key: ApiKey) {
        let key_hash = key
            .key_hash
            .clone()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| api_key_hash(&key.key));

        let _ = sqlx::query(
            r#"
            INSERT INTO api_keys (id, key_hash, agent_id, human_id, scope, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (id) DO UPDATE SET
                key_hash = EXCLUDED.key_hash,
                agent_id = EXCLUDED.agent_id,
                human_id = EXCLUDED.human_id,
                scope = EXCLUDED.scope
            "#,
        )
        .bind(&key.id)
        .bind(&key_hash)
        .bind(&key.agent_id)
        .bind(&key.human_id)
        .bind(key.scope.as_str())
        .execute(&*self.pool)
        .await;
    }

    async fn get_human(&self, id: &str) -> Option<HumanIdentity> {
        self.get_human_inner(id).await
    }

    async fn api_key_scope(&self, id: &str) -> Option<crate::model::identity::KeyScope> {
        let row = sqlx::query("SELECT scope FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_optional(&*self.pool)
            .await
            .ok()??;
        let scope: String = row.try_get("scope").ok()?;
        Some(crate::model::identity::KeyScope::parse(&scope))
    }

    async fn get_agent(&self, id: &str) -> Option<AgentIdentity> {
        let row = sqlx::query(
            "SELECT id, name, client_type, api_key_id, capabilities, metadata, created_at, updated_at \
             FROM agent_identities WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let client_type_str: String = row.try_get("client_type").ok()?;
        let caps_json: serde_json::Value = row.try_get("capabilities").ok()?;
        let meta_json: serde_json::Value = row.try_get("metadata").ok()?;

        Some(AgentIdentity {
            id: row.try_get("id").ok()?,
            name: row.try_get("name").ok()?,
            client_type: match client_type_str.as_str() {
                "codex" => ClientType::Codex,
                "claude_code" => ClientType::ClaudeCode,
                "trae" => ClientType::Trae,
                _ => ClientType::Custom,
            },
            api_key_id: row.try_get("api_key_id").ok()?,
            capabilities: serde_json::from_value(caps_json).unwrap_or_default(),
            metadata: serde_json::from_value(meta_json).unwrap_or_default(),
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
        })
    }

    async fn set_human_status(&self, id: &str, status: HumanStatus) -> bool {
        let status_str = match status {
            HumanStatus::Online => "online",
            HumanStatus::Away => "away",
            HumanStatus::Offline => "offline",
        };
        let result =
            sqlx::query("UPDATE identities SET status = $1, updated_at = now() WHERE id = $2")
                .bind(status_str)
                .bind(id)
                .execute(&*self.pool)
                .await;
        matches!(result, Ok(r) if r.rows_affected() > 0)
    }
}

impl PostgresStore {
    /// Internal: fetch a human identity by ID.
    async fn get_human_inner(&self, id: &str) -> Option<HumanIdentity> {
        let row = sqlx::query(
            "SELECT id, display_name, avatar_url, status, capabilities, metadata, created_at, updated_at \
             FROM identities WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let status_str: String = row.try_get("status").ok()?;
        let caps_json: serde_json::Value = row.try_get("capabilities").ok()?;
        let meta_json: serde_json::Value = row.try_get("metadata").ok()?;

        Some(HumanIdentity {
            id: row.try_get("id").ok()?,
            display_name: row.try_get("display_name").ok()?,
            avatar_url: row.try_get("avatar_url").ok()?,
            status: parse_human_status(&status_str),
            capabilities: serde_json::from_value(caps_json).unwrap_or_default(),
            metadata: serde_json::from_value(meta_json).unwrap_or_default(),
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
        })
    }

    /// Internal: fetch an agent identity by ID.
    async fn get_agent_inner(&self, id: &str) -> Option<AgentIdentity> {
        let row = sqlx::query(
            "SELECT id, name, client_type, api_key_id, capabilities, metadata, created_at, updated_at \
             FROM agent_identities WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let ct_str: String = row.try_get("client_type").ok()?;
        let caps_json: serde_json::Value = row.try_get("capabilities").ok()?;
        let meta_json: serde_json::Value = row.try_get("metadata").ok()?;

        Some(AgentIdentity {
            id: row.try_get("id").ok()?,
            name: row.try_get("name").ok()?,
            client_type: parse_client_type(&ct_str),
            api_key_id: row.try_get("api_key_id").ok()?,
            capabilities: serde_json::from_value(caps_json).unwrap_or_default(),
            metadata: serde_json::from_value(meta_json).unwrap_or_default(),
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
        })
    }
}

// ── ConversationRepository ──

#[async_trait]
impl ConversationRepository for PostgresStore {
    async fn find_or_create(
        &self,
        agent_id: &str,
        human_id: &str,
        session_key: &str,
        system_prompt: Option<String>,
    ) -> Conversation {
        // All three dimensions participate in the lookup: agent + human +
        // session. The previous implementation matched on agent alone, which
        // collapsed every chat window (and, in multi-human setups, every
        // human) onto one shared transcript.
        let existing = self
            .find_conversation_scoped(agent_id, human_id, session_key)
            .await;
        if let Some(cnv) = existing {
            return cnv;
        }

        // Create a new conversation.
        let now = Utc::now();
        let conversation_id = format!("cnv_{}", Uuid::now_v7().simple());

        // Build metadata with optional system_prompt — plus the session key,
        // which is how this conversation is matched on subsequent requests.
        let metadata: serde_json::Value = {
            let mut m = serde_json::Map::new();
            if let Some(prompt) = system_prompt {
                m.insert(
                    "system_prompt".to_string(),
                    serde_json::Value::String(prompt),
                );
            }
            m.insert(
                "session_key".to_string(),
                serde_json::Value::String(session_key.to_string()),
            );
            serde_json::Value::Object(m)
        };

        // Insert conversation.
        let _ = sqlx::query(
            r#"
            INSERT INTO conversations (id, type, title, status, metadata, created_at, updated_at, last_message_at)
            VALUES ($1, 'agent_human', NULL, 'active', $2, $3, $4, NULL)
            "#,
        )
        .bind(&conversation_id)
        .bind(&metadata)
        .bind(now)
        .bind(now)
        .execute(&*self.pool)
        .await;

        // Insert participants.
        let _ = sqlx::query(
            r#"
            INSERT INTO conversation_participants (conversation_id, participant_id, participant_type, joined_at, role)
            VALUES ($1, $2, 'agent', $3, 'initiator')
            "#,
        )
        .bind(&conversation_id)
        .bind(agent_id)
        .bind(now)
        .execute(&*self.pool)
        .await;

        let _ = sqlx::query(
            r#"
            INSERT INTO conversation_participants (conversation_id, participant_id, participant_type, joined_at, role)
            VALUES ($1, $2, 'human', $3, 'responder')
            "#,
        )
        .bind(&conversation_id)
        .bind(human_id)
        .bind(now)
        .execute(&*self.pool)
        .await;

        // Return the conversation object.
        Conversation {
            id: conversation_id,
            conversation_type: ConversationType::AgentHuman,
            participants: vec![
                ConversationParticipant {
                    participant: ParticipantRef::agent(agent_id),
                    joined_at: now,
                    role: Some(ParticipantRole::Initiator),
                },
                ConversationParticipant {
                    participant: ParticipantRef::human(human_id),
                    joined_at: now,
                    role: Some(ParticipantRole::Responder),
                },
            ],
            title: None,
            status: ConversationStatus::Active,
            metadata: {
                let mut m = HashMap::new();
                if let serde_json::Value::Object(map) = &metadata {
                    for (k, v) in map {
                        m.insert(k.clone(), v.clone());
                    }
                }
                m
            },
            created_at: now,
            updated_at: now,
            last_message_at: None,
            deleted_at: None,
        }
    }

    async fn get(&self, id: &str) -> Option<Conversation> {
        let row = sqlx::query(
            "SELECT id, type, title, status, metadata, created_at, updated_at, last_message_at \
             FROM conversations WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let conversation_id: String = row.try_get("id").ok()?;
        let participants = self.get_participants(&conversation_id).await;

        Some(Conversation {
            id: conversation_id,
            conversation_type: ConversationType::AgentHuman,
            participants,
            title: row.try_get("title").ok()?,
            status: ConversationStatus::Active,
            deleted_at: row.try_get("deleted_at").ok().flatten(),
            metadata: {
                let v: serde_json::Value = row.try_get("metadata").ok()?;
                serde_json::from_value(v).unwrap_or_default()
            },
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
            last_message_at: row.try_get("last_message_at").ok()?,
        })
    }

    async fn get_by_agent(&self, agent_id: &str) -> Option<Conversation> {
        self.find_conversation_by_agent(agent_id).await
    }

    async fn list_for_human(&self, human_id: &str) -> Vec<Conversation> {
        // Find all conversation IDs where this human is a participant.
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT c.id, c.title, c.status, c.metadata, c.created_at, c.updated_at, c.last_message_at
            FROM conversations c
            JOIN conversation_participants cp ON cp.conversation_id = c.id
            WHERE cp.participant_id = $1 AND cp.participant_type = 'human'
              AND c.deleted_at IS NULL
            ORDER BY c.last_message_at DESC NULLS LAST
            "#,
        )
        .bind(human_id)
        .fetch_all(&*self.pool)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut conversations = Vec::new();
        for row in &rows {
            if let Ok(conversation_id) = row.try_get::<String, _>("id") {
                let participants = self.get_participants(&conversation_id).await;
                conversations.push(Conversation {
                    id: conversation_id,
                    conversation_type: ConversationType::AgentHuman,
                    participants,
                    title: row.try_get("title").ok(),
                    status: ConversationStatus::Active,
                    metadata: {
                        let v: serde_json::Value = row.try_get("metadata").unwrap_or_default();
                        serde_json::from_value(v).unwrap_or_default()
                    },
                    created_at: row.try_get("created_at").unwrap_or_else(|_| Utc::now()),
                    updated_at: row.try_get("updated_at").unwrap_or_else(|_| Utc::now()),
                    last_message_at: row.try_get("last_message_at").ok(),
                    deleted_at: row.try_get("deleted_at").ok().flatten(),
                });
            }
        }

        conversations
    }

    async fn soft_delete(&self, id: &str) -> bool {
        let result = sqlx::query(
            "UPDATE conversations SET deleted_at = now(), updated_at = now() \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&*self.pool)
        .await;

        matches!(result, Ok(r) if r.rows_affected() > 0)
    }

    async fn restore(&self, id: &str) -> bool {
        let result = sqlx::query(
            "UPDATE conversations SET deleted_at = NULL, updated_at = now() \
             WHERE id = $1 AND deleted_at IS NOT NULL",
        )
        .bind(id)
        .execute(&*self.pool)
        .await;

        matches!(result, Ok(r) if r.rows_affected() > 0)
    }

    async fn delete_permanent(&self, id: &str) -> bool {
        // Only ever reached from the trash. Children go first: participants
        // and messages reference the conversation, so removing the parent
        // row while they remain would leave the rest dangling.
        let _ = sqlx::query("DELETE FROM messages WHERE conversation_id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await;
        let _ = sqlx::query("DELETE FROM conversation_participants WHERE conversation_id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await;

        let result = sqlx::query("DELETE FROM conversations WHERE id = $1")
            .bind(id)
            .execute(&*self.pool)
            .await;

        matches!(result, Ok(r) if r.rows_affected() > 0)
    }

    async fn list_deleted_for_human(&self, human_id: &str) -> Vec<Conversation> {
        // `deleted_at` is selected because DISTINCT requires ORDER BY
        // expressions to appear in the select list — leaving it out makes
        // Postgres reject the query outright, which silently returned an
        // empty trash.
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT c.id, c.title, c.status, c.metadata, c.created_at,
                            c.updated_at, c.last_message_at, c.deleted_at
            FROM conversations c
            JOIN conversation_participants cp ON cp.conversation_id = c.id
            WHERE cp.participant_id = $1 AND cp.participant_type = 'human'
              AND c.deleted_at IS NOT NULL
            ORDER BY c.deleted_at DESC
            "#,
        )
        .bind(human_id)
        .fetch_all(&*self.pool)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("list_deleted_for_human failed: {e}");
                return Vec::new();
            }
        };

        let mut conversations = Vec::new();
        for row in &rows {
            if let Ok(conversation_id) = row.try_get::<String, _>("id") {
                let participants = self.get_participants(&conversation_id).await;
                conversations.push(Conversation {
                    id: conversation_id,
                    conversation_type: ConversationType::AgentHuman,
                    participants,
                    title: row.try_get("title").ok(),
                    status: ConversationStatus::Active,
                    metadata: {
                        let v: serde_json::Value = row.try_get("metadata").unwrap_or_default();
                        serde_json::from_value(v).unwrap_or_default()
                    },
                    created_at: row.try_get("created_at").unwrap_or_else(|_| Utc::now()),
                    updated_at: row.try_get("updated_at").unwrap_or_else(|_| Utc::now()),
                    last_message_at: row.try_get("last_message_at").ok(),
                    deleted_at: row.try_get("deleted_at").ok().flatten(),
                });
            }
        }

        conversations
    }
}

impl PostgresStore {
    /// Internal: find the conversation scoped to `(agent, human, session_key)`.
    ///
    /// Requires BOTH participants to match — the previous
    /// `find_conversation_by_agent` matched on the agent alone, so one API
    /// key produced exactly one conversation no matter which human or which
    /// chat window was talking.
    ///
    /// `session_key` is matched against `metadata->>'session_key'`. Rows
    /// written before that dimension existed hold `NULL` and therefore never
    /// match, so legacy transcripts stay intact and untouched instead of
    /// silently absorbing new traffic.
    async fn find_conversation_scoped(
        &self,
        agent_id: &str,
        human_id: &str,
        session_key: &str,
    ) -> Option<Conversation> {
        let row = sqlx::query(
            r#"
            SELECT c.id, c.type, c.title, c.status, c.metadata, c.created_at, c.updated_at, c.last_message_at
            FROM conversations c
            JOIN conversation_participants cpa
              ON cpa.conversation_id = c.id
             AND cpa.participant_type = 'agent'
             AND cpa.participant_id = $1
            JOIN conversation_participants cph
              ON cph.conversation_id = c.id
             AND cph.participant_type = 'human'
             AND cph.participant_id = $2
            WHERE c.metadata->>'session_key' = $3
            ORDER BY c.created_at DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .bind(human_id)
        .bind(session_key)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let conversation_id: String = row.try_get("id").ok()?;
        let participants = self.get_participants(&conversation_id).await;

        Some(Conversation {
            id: conversation_id,
            conversation_type: ConversationType::AgentHuman,
            participants,
            title: row.try_get("title").ok()?,
            status: ConversationStatus::Active,
            deleted_at: row.try_get("deleted_at").ok().flatten(),
            metadata: {
                let v: serde_json::Value = row.try_get("metadata").ok()?;
                serde_json::from_value(v).unwrap_or_default()
            },
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
            last_message_at: row.try_get("last_message_at").ok()?,
        })
    }

    /// Internal: find a conversation where the given agent is a participant.
    async fn find_conversation_by_agent(&self, agent_id: &str) -> Option<Conversation> {
        let row = sqlx::query(
            r#"
            SELECT c.id, c.type, c.title, c.status, c.metadata, c.created_at, c.updated_at, c.last_message_at
            FROM conversations c
            JOIN conversation_participants cp ON cp.conversation_id = c.id
            WHERE cp.participant_id = $1 AND cp.participant_type = 'agent'
            ORDER BY c.created_at DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let conversation_id: String = row.try_get("id").ok()?;
        let participants = self.get_participants(&conversation_id).await;

        Some(Conversation {
            id: conversation_id,
            conversation_type: ConversationType::AgentHuman,
            participants,
            title: row.try_get("title").ok()?,
            status: ConversationStatus::Active,
            deleted_at: row.try_get("deleted_at").ok().flatten(),
            metadata: {
                let v: serde_json::Value = row.try_get("metadata").ok()?;
                serde_json::from_value(v).unwrap_or_default()
            },
            created_at: row.try_get("created_at").ok()?,
            updated_at: row.try_get("updated_at").ok()?,
            last_message_at: row.try_get("last_message_at").ok()?,
        })
    }

    /// Internal: get all participants for a conversation.
    async fn get_participants(&self, conversation_id: &str) -> Vec<ConversationParticipant> {
        let rows = sqlx::query(
            "SELECT participant_id, participant_type, joined_at, role \
             FROM conversation_participants WHERE conversation_id = $1 ORDER BY joined_at",
        )
        .bind(conversation_id)
        .fetch_all(&*self.pool)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter_map(|r| {
                let pid: String = r.try_get("participant_id").ok()?;
                let ptype_str: String = r.try_get("participant_type").ok()?;
                let joined_at: DateTime<Utc> = r.try_get("joined_at").ok()?;
                let role_str: Option<String> = r.try_get("role").ok()?;

                let participant_type = match ptype_str.as_str() {
                    "human" => ParticipantType::Human,
                    _ => ParticipantType::Agent,
                };

                let role = role_str.as_deref().and_then(|s| match s {
                    "initiator" => Some(ParticipantRole::Initiator),
                    "responder" => Some(ParticipantRole::Responder),
                    "observer" => Some(ParticipantRole::Observer),
                    _ => None,
                });

                Some(ConversationParticipant {
                    participant: ParticipantRef {
                        participant_id: pid,
                        participant_type,
                        display_name: None,
                    },
                    joined_at,
                    role,
                })
            })
            .collect()
    }
}

// ── MessageRepository ──

#[async_trait]
impl MessageRepository for PostgresStore {
    async fn insert(&self, message: Message) {
        let content_json = serde_json::to_value(&message.content).unwrap_or_default();
        let attachments_json = serde_json::to_value(&message.attachments).unwrap_or_default();
        let metadata_json = serde_json::to_value(&message.metadata).unwrap_or_default();
        let sender_type_str = match message.sender.participant_type {
            ParticipantType::Human => "human",
            ParticipantType::Agent => "agent",
        };

        let _ = sqlx::query(
            r#"
            INSERT INTO messages (id, conversation_id, sender_id, sender_type, content, attachments, metadata, status, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(&message.id)
        .bind(&message.conversation_id)
        .bind(&message.sender.participant_id)
        .bind(sender_type_str)
        .bind(&content_json)
        .bind(&attachments_json)
        .bind(&metadata_json)
        .bind(match message.status {
            MessageStatus::Pending => "pending",
            MessageStatus::Sent => "sent",
            MessageStatus::Delivered => "delivered",
            MessageStatus::Read => "read",
            MessageStatus::Failed => "failed",
        })
        .bind(message.created_at)
        .execute(&*self.pool)
        .await;

        // Update last_message_at on the conversation.
        let _ = sqlx::query(
            "UPDATE conversations SET last_message_at = $1, updated_at = $1 WHERE id = $2",
        )
        .bind(message.created_at)
        .bind(&message.conversation_id)
        .execute(&*self.pool)
        .await;
    }

    async fn list(&self, conversation_id: &str) -> Vec<Message> {
        let rows = sqlx::query(
            "SELECT id, conversation_id, sender_id, sender_type, content, attachments, metadata, status, created_at \
             FROM messages WHERE conversation_id = $1 ORDER BY created_at ASC",
        )
        .bind(conversation_id)
        .fetch_all(&*self.pool)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                let cid: String = r.try_get("conversation_id").ok()?;
                let sender_id: String = r.try_get("sender_id").ok()?;
                let sender_type_str: String = r.try_get("sender_type").ok()?;
                let content_json: serde_json::Value = r.try_get("content").ok()?;
                let attachments_json: serde_json::Value = r.try_get("attachments").ok()?;
                let metadata_json: serde_json::Value = r.try_get("metadata").ok()?;
                let status_str: String = r.try_get("status").ok()?;
                let created_at: DateTime<Utc> = r.try_get("created_at").ok()?;

                let sender_type = match sender_type_str.as_str() {
                    "human" => ParticipantType::Human,
                    _ => ParticipantType::Agent,
                };

                let status = match status_str.as_str() {
                    "pending" => MessageStatus::Pending,
                    "sent" => MessageStatus::Sent,
                    "delivered" => MessageStatus::Delivered,
                    "read" => MessageStatus::Read,
                    _ => MessageStatus::Failed,
                };

                let content: ContentBlock =
                    serde_json::from_value(content_json).unwrap_or(ContentBlock::text(""));

                Some(Message {
                    id,
                    conversation_id: cid,
                    sender: ParticipantRef {
                        participant_id: sender_id,
                        participant_type: sender_type,
                        display_name: None,
                    },
                    content,
                    attachments: serde_json::from_value(attachments_json).unwrap_or_default(),
                    metadata: serde_json::from_value(metadata_json).unwrap_or_default(),
                    status,
                    visibility: crate::model::message::MessageVisibility::HumanVisible,
                    created_at,
                })
            })
            .collect()
    }

    async fn list_undelivered(&self, human_id: &str) -> Vec<Message> {
        // Participants live in `conversation_participants` — there is no
        // `human_id` column on `conversations`. The query that assumed one
        // failed on every call, and the failure was swallowed, so the
        // backlog was never drained and messages stayed `pending` forever.
        let rows = sqlx::query(
            r#"
            SELECT m.id, m.conversation_id, m.sender_id, m.sender_type, m.content,
                   m.attachments, m.metadata, m.status, m.created_at
            FROM messages m
            JOIN conversation_participants cp
              ON cp.conversation_id = m.conversation_id
            WHERE cp.participant_id = $1
              AND cp.participant_type = 'human'
              AND m.status = 'pending'
              AND m.sender_id <> $1
            ORDER BY m.created_at ASC
            "#,
        )
        .bind(human_id)
        .fetch_all(&*self.pool)
        .await;

        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                // Never `unwrap_or_default` here: an empty backlog and a
                // failed lookup look identical to the caller, and the
                // latter means messages silently never arrive.
                tracing::error!("list_undelivered failed for {human_id}: {e}");
                return Vec::new();
            }
        };

        rows.into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                let cid: String = r.try_get("conversation_id").ok()?;
                let sender_id: String = r.try_get("sender_id").ok()?;
                let sender_type_str: String = r.try_get("sender_type").ok()?;
                let content_json: serde_json::Value = r.try_get("content").ok()?;
                let attachments_json: serde_json::Value = r.try_get("attachments").ok()?;
                let metadata_json: serde_json::Value = r.try_get("metadata").ok()?;
                let created_at: DateTime<Utc> = r.try_get("created_at").ok()?;

                let sender_type = match sender_type_str.as_str() {
                    "human" => ParticipantType::Human,
                    _ => ParticipantType::Agent,
                };
                let content: ContentBlock =
                    serde_json::from_value(content_json).unwrap_or(ContentBlock::text(""));

                Some(Message {
                    id,
                    conversation_id: cid,
                    sender: ParticipantRef {
                        participant_id: sender_id,
                        participant_type: sender_type,
                        display_name: None,
                    },
                    content,
                    attachments: serde_json::from_value(attachments_json).unwrap_or_default(),
                    metadata: serde_json::from_value(metadata_json).unwrap_or_default(),
                    status: MessageStatus::Pending,
                    visibility: crate::model::message::MessageVisibility::HumanVisible,
                    created_at,
                })
            })
            .collect()
    }

    async fn mark_delivered(&self, message_ids: &[String]) {
        if message_ids.is_empty() {
            return;
        }
        // Only flip messages that are still pending: later statuses (read,
        // failed) carry information this must not overwrite.
        let _ = sqlx::query(
            "UPDATE messages SET status = 'delivered' WHERE id = ANY($1) AND status = 'pending'",
        )
        .bind(message_ids)
        .execute(&*self.pool)
        .await;
    }

    async fn find_tool_call(&self, conversation_id: &str, tool_call_id: &str) -> Option<Message> {
        // Direct lookup on the metadata index — the previous implementation
        // loaded the whole conversation and scanned it in Rust, which gets
        // slower with every message in the thread.
        let row = sqlx::query(
            "SELECT id, conversation_id, sender_id, sender_type, content, attachments, metadata, status, created_at \
             FROM messages \
             WHERE conversation_id = $1 \
               AND metadata->>'tool_call_id' = $2 \
               AND metadata ? 'tool_id' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(conversation_id)
        .bind(tool_call_id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let sender_type_str: String = row.try_get("sender_type").ok()?;
        let content_json: serde_json::Value = row.try_get("content").ok()?;
        let attachments_json: serde_json::Value = row.try_get("attachments").ok()?;
        let metadata_json: serde_json::Value = row.try_get("metadata").ok()?;
        let status_str: String = row.try_get("status").ok()?;

        Some(Message {
            id: row.try_get("id").ok()?,
            conversation_id: row.try_get("conversation_id").ok()?,
            sender: ParticipantRef {
                participant_id: row.try_get("sender_id").ok()?,
                participant_type: match sender_type_str.as_str() {
                    "human" => ParticipantType::Human,
                    _ => ParticipantType::Agent,
                },
                display_name: None,
            },
            content: serde_json::from_value(content_json).unwrap_or(ContentBlock::text("")),
            attachments: serde_json::from_value(attachments_json).unwrap_or_default(),
            metadata: serde_json::from_value(metadata_json).unwrap_or_default(),
            status: match status_str.as_str() {
                "pending" => MessageStatus::Pending,
                "sent" => MessageStatus::Sent,
                "delivered" => MessageStatus::Delivered,
                "read" => MessageStatus::Read,
                _ => MessageStatus::Failed,
            },
            visibility: crate::model::message::MessageVisibility::HumanVisible,
            created_at: row.try_get("created_at").ok()?,
        })
    }
}

// ── CapabilityRepository (Phase 7) ──

#[async_trait]
impl CapabilityRepository for PostgresStore {
    async fn set_capabilities(&self, agent_id: &str, capabilities: Vec<AgentCapability>) {
        // Delete existing capabilities, then insert new ones (upsert manifest).
        let _ = sqlx::query("DELETE FROM agent_capabilities WHERE agent_id = $1")
            .bind(agent_id)
            .execute(&*self.pool)
            .await;

        for cap in &capabilities {
            let _ = sqlx::query(
                r#"
                INSERT INTO agent_capabilities
                    (agent_id, id, name, description, category,
                     input_schema, output_schema, ui_schema,
                     permissions, metadata, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
                ON CONFLICT (agent_id, id) DO UPDATE SET
                    name          = EXCLUDED.name,
                    description   = EXCLUDED.description,
                    category      = EXCLUDED.category,
                    input_schema  = EXCLUDED.input_schema,
                    output_schema = EXCLUDED.output_schema,
                    ui_schema     = EXCLUDED.ui_schema,
                    permissions   = EXCLUDED.permissions,
                    metadata      = EXCLUDED.metadata,
                    updated_at    = NOW()
                "#,
            )
            .bind(agent_id)
            .bind(&cap.id)
            .bind(&cap.name)
            .bind(&cap.description)
            .bind(&cap.category)
            .bind(&cap.input_schema)
            .bind(&cap.output_schema)
            .bind(&cap.ui_schema)
            .bind(serde_json::to_value(&cap.permissions).unwrap_or_default())
            .bind(serde_json::to_value(&cap.metadata).unwrap_or_default())
            .execute(&*self.pool)
            .await;
        }
    }

    async fn get_capabilities(&self, agent_id: &str) -> Vec<AgentCapability> {
        let rows = sqlx::query(
            "SELECT id, name, description, category, input_schema, output_schema, \
             ui_schema, permissions, metadata \
             FROM agent_capabilities WHERE agent_id = $1 ORDER BY id",
        )
        .bind(agent_id)
        .fetch_all(&*self.pool)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter_map(|r| {
                let perms_json: serde_json::Value = r.try_get("permissions").ok()?;
                let meta_json: serde_json::Value = r.try_get("metadata").ok()?;
                Some(AgentCapability {
                    id: r.try_get("id").ok()?,
                    name: r.try_get("name").ok()?,
                    description: r.try_get("description").ok()?,
                    category: r.try_get("category").ok()?,
                    input_schema: r.try_get("input_schema").ok()?,
                    output_schema: r.try_get("output_schema").ok()?,
                    ui_schema: r.try_get("ui_schema").ok()?,
                    permissions: serde_json::from_value(perms_json).unwrap_or_default(),
                    metadata: serde_json::from_value(meta_json).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn get_capability(&self, agent_id: &str, tool_id: &str) -> Option<AgentCapability> {
        let row = sqlx::query(
            "SELECT id, name, description, category, input_schema, output_schema, \
             ui_schema, permissions, metadata \
             FROM agent_capabilities WHERE agent_id = $1 AND id = $2",
        )
        .bind(agent_id)
        .bind(tool_id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        let perms_json: serde_json::Value = row.try_get("permissions").ok()?;
        let meta_json: serde_json::Value = row.try_get("metadata").ok()?;
        Some(AgentCapability {
            id: row.try_get("id").ok()?,
            name: row.try_get("name").ok()?,
            description: row.try_get("description").ok()?,
            category: row.try_get("category").ok()?,
            input_schema: row.try_get("input_schema").ok()?,
            output_schema: row.try_get("output_schema").ok()?,
            ui_schema: row.try_get("ui_schema").ok()?,
            permissions: serde_json::from_value(perms_json).unwrap_or_default(),
            metadata: serde_json::from_value(meta_json).unwrap_or_default(),
        })
    }

    async fn clear_capabilities(&self, agent_id: &str) {
        let _ = sqlx::query("DELETE FROM agent_capabilities WHERE agent_id = $1")
            .bind(agent_id)
            .execute(&*self.pool)
            .await;
    }
}

// ── ContextRepository (Phase 7) ──

#[async_trait]
impl ContextRepository for PostgresStore {
    async fn set_context(&self, agent_id: &str, context: AgentContext) {
        let _ = sqlx::query(
            r#"
            INSERT INTO agent_contexts
                (agent_id, system_prompt, model, runtime, environment, updated_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (agent_id) DO UPDATE SET
                system_prompt = EXCLUDED.system_prompt,
                model         = EXCLUDED.model,
                runtime       = EXCLUDED.runtime,
                environment   = EXCLUDED.environment,
                updated_at    = NOW()
            "#,
        )
        .bind(agent_id)
        .bind(&context.system_prompt)
        .bind(&context.model)
        .bind(&context.runtime)
        .bind(&context.environment)
        .execute(&*self.pool)
        .await;
    }

    async fn get_context(&self, agent_id: &str) -> Option<AgentContext> {
        let row = sqlx::query(
            "SELECT agent_id, system_prompt, model, runtime, environment \
             FROM agent_contexts WHERE agent_id = $1",
        )
        .bind(agent_id)
        .fetch_optional(&*self.pool)
        .await
        .ok()??;

        Some(AgentContext {
            agent_id: row.try_get("agent_id").ok()?,
            system_prompt: row.try_get("system_prompt").ok()?,
            model: row.try_get("model").ok()?,
            runtime: row.try_get("runtime").ok()?,
            environment: row.try_get("environment").ok()?,
        })
    }
}

// ════════════════════════════════════════════════════════════
//  InMemoryReplyRegistry
// ════════════════════════════════════════════════════════════

/// Ephemeral in-memory reply channel registry.
///
/// This is always in-memory because it carries live `mpsc::Sender` objects
/// that cannot be serialized to a database. Even with PostgreSQL persistence,
/// the reply bridge between SSE and WebSocket is inherently ephemeral.
#[derive(Default)]
struct ReplyData {
    pending: HashMap<String, Vec<mpsc::UnboundedSender<String>>>,
}

/// Thread-safe in-memory reply registry (Clone is cheap — inner `Arc`).
#[derive(Clone)]
pub struct InMemoryReplyRegistry {
    data: Arc<Mutex<ReplyData>>,
}

impl InMemoryReplyRegistry {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(ReplyData::default())),
        }
    }
}

impl Default for InMemoryReplyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ReplyRegistry for InMemoryReplyRegistry {
    async fn register(&self, conversation_id: &str, tx: mpsc::UnboundedSender<String>) {
        let mut data = self.data.lock().await;
        // The newest request on a conversation supersedes any older one: an
        // older one still open is a caller that resubmitted (a retry, or a
        // proxy replaying the request). Dropping its sender ends that
        // handler quietly instead of refusing the new request with 409.
        data.pending.insert(conversation_id.to_string(), vec![tx]);
    }

    async fn remove(&self, conversation_id: &str) {
        let mut data = self.data.lock().await;
        // A superseded handler sees its channel closed and exits without
        // calling this, so removing everything here only ends the turn
        // that actually finished.
        data.pending.remove(conversation_id);
    }

    async fn deliver(&self, conversation_id: &str, text: &str) -> bool {
        let mut data = self.data.lock().await;
        if let Some(channels) = data.pending.get_mut(conversation_id) {
            // Drop channels whose request already went away (client
            // disconnected mid-stream), then deliver to the newest live one.
            channels.retain(|tx| !tx.is_closed());
            if let Some(tx) = channels.last() {
                let _ = tx.send(text.to_string());
                return true;
            }
        }
        false
    }

    async fn conversations_with_waiters(&self) -> Vec<String> {
        let data = self.data.lock().await;
        data.pending
            .iter()
            .filter(|(_, channels)| channels.iter().any(|tx| !tx.is_closed()))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

// ════════════════════════════════════════════════════════════
//  Helpers
// ════════════════════════════════════════════════════════════

/// SHA-256 hex digest of an API key — the persisted, indexed form.
///
/// Previously a `DefaultHasher` placeholder lived here. It was 64-bit,
/// non-cryptographic, and **not stable across process restarts**, so it
/// could not serve as a lookup key at all — authentication silently fell
/// back to `WHERE raw_key = $1`. See `crate::crypto` for the rationale.
///
/// A plain (unsalted) digest is deliberate: lookups are exact-match, so a
/// per-install salt would invalidate every issued key on migration. API
/// keys are high-entropy random strings, so they are not exposed to the
/// rainbow-table weakness that unsalted password hashes suffer from.
fn api_key_hash(raw_key: &str) -> String {
    crate::crypto::sha256_hex(raw_key)
}

/// Parse a human status string from the DB.
fn parse_human_status(s: &str) -> Option<HumanStatus> {
    match s {
        "online" => Some(HumanStatus::Online),
        "away" => Some(HumanStatus::Away),
        "offline" => Some(HumanStatus::Offline),
        _ => None,
    }
}

/// Parse a client type string from the DB.
fn parse_client_type(s: &str) -> ClientType {
    match s {
        "codex" => ClientType::Codex,
        "claude_code" => ClientType::ClaudeCode,
        "trae" => ClientType::Trae,
        _ => ClientType::Custom,
    }
}
