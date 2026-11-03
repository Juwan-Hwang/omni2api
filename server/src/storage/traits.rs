//! Repository trait definitions — the storage abstraction layer.
//!
//! These traits define the contract between the service layer and the
//! storage layer. The current implementation is `MemoryStore`, but the
//! same traits will be implemented by a SQLx-backed PostgreSQL store
//! in a future phase.
//!
//! Design principles:
//! - All methods are `async` (use `#[async_trait]`)
//! - Methods take `&self` or `&mut self` — the concrete store manages
//!   interior mutability (e.g., `Mutex<Store>` or `PgPool`)
//! - All return `Option` or `Result` — no panics
//! - Types from `crate::model` are used directly — no storage-specific types

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::model::agent_capability::AgentCapability;
use crate::model::agent_context::AgentContext;
use crate::model::conversation::Conversation;
use crate::model::identity::{AgentIdentity, ApiKey, HumanIdentity};
use crate::model::message::Message;
use crate::state::AuthPrincipal;

// ── Identity Repository ──

/// Identity storage — humans, agents, and API key resolution.
#[async_trait]
pub trait IdentityRepository: Send + Sync {
    /// Resolve an API key string into the full principal
    /// (agent + human + key record).
    async fn resolve_principal(&self, raw_key: &str) -> Option<AuthPrincipal>;

    /// Insert a human identity.
    async fn insert_human(&self, human: HumanIdentity);

    /// Insert an agent identity.
    async fn insert_agent(&self, agent: AgentIdentity);

    /// Insert an API key.
    async fn insert_api_key(&self, key: ApiKey);

    /// The current scope of a key, by its id; `None` when no such key
    /// exists. Credentials derived from a key (a ChatKit client secret)
    /// check this on every use, so revoking or deleting the key cuts them
    /// off at once instead of at their expiry. The default knows no key,
    /// so a backend that does not implement it fails closed.
    async fn api_key_scope(&self, id: &str) -> Option<crate::model::identity::KeyScope> {
        let _ = id;
        None
    }

    /// Get a human by ID.
    async fn get_human(&self, id: &str) -> Option<HumanIdentity>;

    /// Get an agent by ID.
    ///
    /// History listings want the agent's display name; without it a
    /// console can only show the raw `agt_...` id.
    async fn get_agent(&self, id: &str) -> Option<AgentIdentity> {
        let _ = id;
        None
    }

    /// Record a human's presence status.
    ///
    /// Called when the console sends `presence.update`. The status is what
    /// lets an agent (or a second device) tell "away" from "offline"
    /// without guessing from socket state.
    async fn set_human_status(
        &self,
        id: &str,
        status: crate::model::identity::HumanStatus,
    ) -> bool {
        let _ = (id, status);
        false
    }

    /// Rename an agent, restricted to agents this human actually talks to.
    ///
    /// The seeded name is rarely what the human calls the thing. Scoping
    /// the update to the caller's own agents keeps one key from renaming
    /// somebody else's.
    /// `api_key_id` is the key presenting the request.
    ///
    /// Checking only that *some* key pairs this agent with this human is
    /// not enough: two different agents' keys can belong to the same
    /// human, and either could then rename the other. The presented key
    /// has to be the one the agent is issued under.
    async fn rename_agent(
        &self,
        agent_id: &str,
        human_id: &str,
        api_key_id: &str,
        name: &str,
    ) -> Option<AgentIdentity> {
        let _ = (agent_id, human_id, api_key_id, name);
        None
    }
}

// ── Conversation Repository ──

/// Conversation storage — find, create, and query conversations.
#[async_trait]
pub trait ConversationRepository: Send + Sync {
    /// Find or create the conversation scoped to `(agent_id, human_id, session_key)`.
    ///
    /// **All three dimensions participate in the lookup.** Historically only
    /// `agent_id` (Postgres) or `(agent_id, human_id)` (memory) participated,
    /// which collapsed every chat window opened with the same API key onto a
    /// single shared transcript — see the `session_key` note below.
    ///
    /// `session_key` is derived by [`crate::api::openai::resolve_session_key`]
    /// from, in priority order: an explicit conversation id from the caller
    /// (header or request body), the OpenAI `user` field, and finally a
    /// fingerprint of the system prompt. It is never empty — the caller
    /// guarantees a non-empty value so that legacy conversations written
    /// before this dimension existed never match.
    async fn find_or_create(
        &self,
        agent_id: &str,
        human_id: &str,
        session_key: &str,
        system_prompt: Option<String>,
    ) -> Conversation;

    /// Get a conversation by ID.
    async fn get(&self, id: &str) -> Option<Conversation>;

    /// Find a conversation by its agent participant ID.
    async fn get_by_agent(&self, agent_id: &str) -> Option<Conversation>;

    /// List all conversations where the given human is a participant.
    ///
    /// Used by the WS handler to drain pending messages across all
    /// conversations for a human (not just one hardcoded agent).
    ///
    /// Deleted conversations are excluded — see [Self::list_deleted].
    async fn list_for_human(&self, human_id: &str) -> Vec<Conversation>;

    /// Move a conversation to the trash.
    ///
    /// The row and its messages survive; only `deleted_at` is stamped. A
    /// delete that destroyed the transcript outright left no way back.
    async fn soft_delete(&self, id: &str) -> bool;

    /// Take a conversation back out of the trash.
    async fn restore(&self, id: &str) -> bool;

    /// Destroy a conversation and everything in it. Only for the trash.
    async fn delete_permanent(&self, id: &str) -> bool;

    /// Conversations this human has discarded, most recently first.
    async fn list_deleted_for_human(&self, human_id: &str) -> Vec<Conversation>;
}

// ── Message Repository ──

/// Message storage — append and query messages within conversations.
#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Append a message to a conversation.
    async fn insert(&self, message: Message);

    /// Get all messages in a conversation (chronological order).
    async fn list(&self, conversation_id: &str) -> Vec<Message>;

    /// Messages for a human that have not reached a device yet.
    ///
    /// Draining the backlog on connect is how an offline human catches up,
    /// but replaying *everything* each time means every reconnect re-shows
    /// the whole history — and on a long-lived conversation that is enough
    /// data to swamp the connection.
    async fn list_undelivered(&self, human_id: &str) -> Vec<Message> {
        let _ = human_id;
        Vec::new()
    }

    /// Mark messages as delivered to a device.
    ///
    /// Only the ids actually pushed should be passed; marking anything else
    /// would silently drop messages the human never saw.
    async fn mark_delivered(&self, _message_ids: &[String]) {}

    /// Find the stored tool call a `role: "tool"` result answers.
    ///
    /// The agent only echoes `tool_call_id`; this recovers the originating
    /// call (and its `tool_id`) so the result can be routed to the exact
    /// card the human issued. Returns `None` for unknown ids.
    async fn find_tool_call(&self, conversation_id: &str, tool_call_id: &str) -> Option<Message> {
        let _ = (conversation_id, tool_call_id);
        None
    }
}

// ── Capability Repository (Phase 7) ──

/// Storage for agent capabilities — tools an agent exposes to humans.
///
/// When an agent publishes a `capability.manifest`, the server upserts
/// all capabilities here. Human clients query this to render the
/// agent's tool list in the UI.
#[async_trait]
pub trait CapabilityRepository: Send + Sync {
    /// Replace all capabilities for an agent (upsert on manifest publish).
    async fn set_capabilities(&self, agent_id: &str, capabilities: Vec<AgentCapability>);

    /// Get all capabilities for an agent.
    async fn get_capabilities(&self, agent_id: &str) -> Vec<AgentCapability>;

    /// Get a single capability by agent + tool ID.
    async fn get_capability(&self, agent_id: &str, tool_id: &str) -> Option<AgentCapability>;

    /// Remove all capabilities for an agent (on disconnect / manifest clear).
    async fn clear_capabilities(&self, agent_id: &str);
}

// ── Context Repository (Phase 7) ──

/// Storage for agent context — non-sensitive runtime metadata.
///
/// Updated when an agent publishes a `capability.manifest` with a
/// context block. Human clients use this to display "Agent is using
/// GPT-4o on branch main" style information.
#[async_trait]
pub trait ContextRepository: Send + Sync {
    /// Upsert the context for an agent.
    async fn set_context(&self, agent_id: &str, context: AgentContext);

    /// Get the context for an agent.
    async fn get_context(&self, agent_id: &str) -> Option<AgentContext>;
}

// ── Reply Registry ──

/// Pending reply channel registry — bridges the mock endpoint
/// (or future WebSocket) to the waiting SSE/HTTP handler.
///
/// This is the only piece of state that is inherently ephemeral
/// (in-memory). Even with PostgreSQL, the reply channel will
/// always be in-memory because it carries a live `mpsc::Sender`.
#[async_trait]
pub trait ReplyRegistry: Send + Sync {
    /// Register a channel for receiving the human's reply.
    async fn register(&self, conversation_id: &str, tx: mpsc::UnboundedSender<String>);

    /// Remove a pending reply channel.
    async fn remove(&self, conversation_id: &str);

    /// Deliver a reply to the waiting handler. Returns `false` if
    /// no one is waiting.
    async fn deliver(&self, conversation_id: &str, text: &str) -> bool;

    /// Conversations that currently have a request waiting on them.
    ///
    /// Lets the server announce presence changes only to turns that are
    /// actually open, rather than broadcasting to every conversation the
    /// human has ever had. Deliberately required rather than defaulted: a
    /// silent empty default would make presence announcements quietly do
    /// nothing, which is far worse than a compile error.
    async fn conversations_with_waiters(&self) -> Vec<String>;

    /// Whether a request is already waiting on this conversation.
    ///
    /// A second concurrent turn on one conversation cannot work: replies
    /// are routed per conversation, so the two requests would race for the
    /// same answer and one of them would hang until timeout. Callers use
    /// this to refuse the second turn with `409` instead.
    async fn has_waiter(&self, conversation_id: &str) -> bool {
        self.conversations_with_waiters()
            .await
            .iter()
            .any(|id| id == conversation_id)
    }
}
