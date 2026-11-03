//! Integration tests for the PostgreSQL storage backend.
//!
//! These tests verify that `PostgresStore` correctly implements all four
//! repository traits, matching the behavior of `MemoryStore`.
//!
//! # Requirements
//!
//! - A running PostgreSQL instance
//! - `DATABASE_URL` env var set to a valid connection string
//! - The test will run migrations (create tables + seed data) automatically
//!
//! Run with:
//! ```sh
//! $env:DATABASE_URL = "postgres://omni2api:omni2api@localhost:5432/omni2api"
//! cargo test --test postgres_compat -- --ignored
//! ```

use omni2api_server::storage::postgres::{InMemoryReplyRegistry, PostgresStore};
use omni2api_server::storage::traits::{
    ConversationRepository, IdentityRepository, MessageRepository, ReplyRegistry,
};

use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

use omni2api_server::model::content::ContentBlock;
use omni2api_server::model::identity::{
    AgentIdentity, ApiKey, ClientType, HumanIdentity, HumanStatus, ParticipantRef,
};
use omni2api_server::model::message::{Message, MessageStatus, MessageVisibility};

/// The PG tests share one database and each wipes its tables
/// (`clean_db`), so two running at once delete each other's rows. Every
/// test holds this lock for its whole body: they run one at a time, while
/// the rest of the suite stays parallel.
static PG_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A store plus the serial lock, held until the test ends.
struct PgTest {
    _serial: tokio::sync::MutexGuard<'static, ()>,
    store: PostgresStore,
}

impl std::ops::Deref for PgTest {
    type Target = PostgresStore;
    fn deref(&self) -> &PostgresStore {
        &self.store
    }
}

/// Skip tests if DATABASE_URL is not set or PostgreSQL is not available.
async fn get_store() -> Option<PgTest> {
    let db_url = std::env::var("DATABASE_URL").ok()?;
    let serial = PG_SERIAL.lock().await;
    match PostgresStore::new(&db_url).await {
        Ok(store) => {
            // Run migrations (creates tables + seeds demo data).
            // If migrations fail, the tables might already exist — that's OK.
            let _ = store.migrate().await;
            Some(PgTest {
                _serial: serial,
                store,
            })
        }
        Err(_) => None,
    }
}

/// Clean up all tables between tests.
async fn clean_db(store: &PostgresStore) {
    let pool = store.pool();
    let _ = sqlx::query("DELETE FROM messages").execute(pool).await;
    let _ = sqlx::query("DELETE FROM conversation_participants")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM conversations").execute(pool).await;
    let _ = sqlx::query("DELETE FROM api_keys").execute(pool).await;
    let _ = sqlx::query("DELETE FROM agent_identities")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM identities").execute(pool).await;
}

// ── 1. Identity resolution ──

#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_resolve_principal() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    // The migration seeds the demo key.
    let principal = store.resolve_principal("ohk_demo_alice_codex").await;

    assert!(principal.is_some(), "Demo key should resolve");
    let p = principal.unwrap();
    assert_eq!(p.agent.id, "agt_demo_codex");
    assert_eq!(p.agent.name, "Codex Workspace");
    assert_eq!(p.human.id, "hum_demo_alice");
    assert_eq!(p.human.display_name, "Alice Chen");
    assert_eq!(p.api_key.id, "key_demo_001");
}

#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_resolve_principal_invalid_key() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    let principal = store.resolve_principal("ohk_invalid_key").await;
    assert!(principal.is_none(), "Invalid key should not resolve");
}

// ── 2. Conversation find_or_create ──

#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_find_or_create_conversation() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    clean_db(&store).await;

    // First call creates a new conversation.
    let conv1 = store
        .find_or_create(
            "agt_demo_codex",
            "hum_demo_alice",
            "ses:window-a",
            Some("You are a reviewer.".to_string()),
        )
        .await;

    assert!(conv1.id.starts_with("cnv_"));
    assert_eq!(conv1.participants.len(), 2);
    assert_eq!(
        conv1.participants[0].participant.participant_id,
        "agt_demo_codex"
    );
    assert_eq!(
        conv1.participants[1].participant.participant_id,
        "hum_demo_alice"
    );
    assert_eq!(
        conv1.metadata.get("system_prompt"),
        Some(&serde_json::Value::String(
            "You are a reviewer.".to_string()
        ))
    );

    // Second call with the SAME session returns the same conversation.
    let conv2 = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:window-a", None)
        .await;

    assert_eq!(conv1.id, conv2.id, "Should return same conversation");

    clean_db(&store).await;
}

/// Regression: a different session key must produce a different
/// conversation, even for the same agent + human pair.
///
/// Previously `find_or_create` matched on `agent_id` alone, so every chat
/// window opened with one API key collapsed onto a single transcript.
#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_conversation_scoped_by_session() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    clean_db(&store).await;

    let window_a = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:window-a", None)
        .await;
    let window_b = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:window-b", None)
        .await;

    assert_ne!(
        window_a.id, window_b.id,
        "Different chat windows must not share a conversation"
    );

    // Repeating a key is still stable — the scope is not "always new".
    let window_a_again = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:window-a", None)
        .await;
    assert_eq!(window_a.id, window_a_again.id);

    clean_db(&store).await;
}

/// Regression: conversations must not be shared across humans either.
///
/// The old lookup ignored `human_id` entirely, so a second human bound to
/// the same agent would inherit the first human's transcript.
#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_conversation_scoped_by_human() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    clean_db(&store).await;

    // Seed a second human so the FK on conversation_participants holds.
    let now = chrono::Utc::now();
    store
        .insert_human(omni2api_server::model::identity::HumanIdentity {
            id: "hum_demo_bob".to_string(),
            display_name: "Bob".to_string(),
            avatar_url: None,
            status: None,
            capabilities: vec![],
            metadata: std::collections::HashMap::new(),
            created_at: now,
            updated_at: now,
        })
        .await;

    let alice = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:window-a", None)
        .await;
    let bob = store
        .find_or_create("agt_demo_codex", "hum_demo_bob", "ses:window-a", None)
        .await;

    assert_ne!(
        alice.id, bob.id,
        "Same session key for different humans must not share a conversation"
    );

    clean_db(&store).await;
}

// ── 3. Message insert and list ──

#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_message_insert_and_list() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    clean_db(&store).await;

    let conv = store
        .find_or_create("agt_demo_codex", "hum_demo_alice", "ses:test", None)
        .await;

    let now = Utc::now();
    store
        .insert(Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conv.id.clone(),
            sender: ParticipantRef::agent("agt_demo_codex"),
            content: ContentBlock::text("Hello from agent"),
            attachments: Vec::new(),
            metadata: HashMap::new(),
            status: MessageStatus::Pending,
            visibility: MessageVisibility::HumanVisible,
            created_at: now,
        })
        .await;

    store
        .insert(Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conv.id.clone(),
            sender: ParticipantRef::human("hum_demo_alice"),
            content: ContentBlock::text("Hello from human"),
            attachments: Vec::new(),
            metadata: HashMap::new(),
            status: MessageStatus::Sent,
            visibility: MessageVisibility::HumanVisible,
            created_at: now,
        })
        .await;

    let messages = store.list(&conv.id).await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content.as_text(), Some("Hello from agent"));
    assert_eq!(messages[1].content.as_text(), Some("Hello from human"));

    clean_db(&store).await;
}

// ── 4. ReplyRegistry (InMemoryReplyRegistry) ──

#[tokio::test]
async fn test_in_memory_reply_registry() {
    let registry = InMemoryReplyRegistry::new();

    // No one is waiting — deliver should return false.
    let delivered = registry.deliver("cnv_nonexistent", "reply text").await;
    assert!(!delivered, "Should return false when no one is waiting");

    // Register a channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    registry.register("cnv_test_1", tx).await;

    // Deliver a reply.
    let delivered = registry.deliver("cnv_test_1", "Hello reply").await;
    assert!(delivered, "Should deliver to registered channel");

    // Check the receiver got the text.
    let received = rx.recv().await.unwrap();
    assert_eq!(received, "Hello reply");

    // Remove the channel.
    registry.remove("cnv_test_1").await;

    // Deliver should fail again.
    let delivered = registry.deliver("cnv_test_1", "should not deliver").await;
    assert!(!delivered, "Should return false after remove");
}

#[tokio::test]
async fn resubmitted_turn_supersedes_the_open_one() {
    let registry = InMemoryReplyRegistry::new();
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<String>();
    registry.register("cnv_retry", tx1).await;
    registry.register("cnv_retry", tx2).await;
    // The older request's channel is closed, so its handler exits.
    assert!(rx1.recv().await.is_none());
    assert!(registry.deliver("cnv_retry", "answer").await);
    assert_eq!(rx2.recv().await.unwrap(), "answer");
}

// ── 5. Full chain: insert → list → verify (PG) ──

#[tokio::test]
#[ignore = "Requires PostgreSQL — set DATABASE_URL and run with --ignored"]
async fn test_pg_full_storage_chain() {
    let store = match get_store().await {
        Some(s) => s,
        None => return,
    };

    clean_db(&store).await;

    // 1. Insert identities + key.
    let now = Utc::now();
    store
        .insert_human(HumanIdentity {
            id: "hum_test_001".to_string(),
            display_name: "Test Human".to_string(),
            avatar_url: None,
            status: Some(HumanStatus::Online),
            capabilities: vec!["testing".to_string()],
            metadata: HashMap::new(),
            created_at: now,
            updated_at: now,
        })
        .await;

    store
        .insert_agent(AgentIdentity {
            id: "agt_test_001".to_string(),
            name: "Test Agent".to_string(),
            client_type: ClientType::Codex,
            api_key_id: "key_test_001".to_string(),
            capabilities: vec!["coding".to_string()],
            metadata: HashMap::new(),
            created_at: now,
            updated_at: now,
        })
        .await;

    store
        .insert_api_key(ApiKey::new(
            "key_test_001",
            "ohk_test_key",
            "agt_test_001",
            "hum_test_001",
        ))
        .await;

    // 2. Resolve principal — now by SHA-256 digest, never plaintext.
    let principal = store.resolve_principal("ohk_test_key").await;
    assert!(principal.is_some(), "Should resolve test principal");
    let p = principal.unwrap();
    assert_eq!(p.agent.id, "agt_test_001");
    assert_eq!(p.human.id, "hum_test_001");
    assert_eq!(
        p.api_key.key_hash.as_deref(),
        Some(omni2api_server::crypto::sha256_hex("ohk_test_key").as_str()),
        "Resolved principal must carry the SHA-256 digest"
    );

    // 2b. A wrong key must not resolve.
    assert!(
        store.resolve_principal("ohk_wrong_key").await.is_none(),
        "An unknown key must not resolve"
    );

    // 3. Create conversation.
    let conv = store
        .find_or_create(
            "agt_test_001",
            "hum_test_001",
            "ses:test",
            Some("System prompt".to_string()),
        )
        .await;

    // 4. Insert messages.
    store
        .insert(Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conv.id.clone(),
            sender: ParticipantRef::agent("agt_test_001"),
            content: ContentBlock::text("Review this"),
            attachments: Vec::new(),
            metadata: HashMap::new(),
            status: MessageStatus::Pending,
            visibility: MessageVisibility::HumanVisible,
            created_at: now,
        })
        .await;

    store
        .insert(Message {
            id: format!("msg_{}", Uuid::now_v7().simple()),
            conversation_id: conv.id.clone(),
            sender: ParticipantRef::human("hum_test_001"),
            content: ContentBlock::text("Looks good"),
            attachments: Vec::new(),
            metadata: HashMap::new(),
            status: MessageStatus::Sent,
            visibility: MessageVisibility::HumanVisible,
            created_at: now,
        })
        .await;

    // 5. List messages.
    let messages = store.list(&conv.id).await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content.as_text(), Some("Review this"));
    assert_eq!(messages[1].content.as_text(), Some("Looks good"));

    // 6. Get conversation by agent.
    let fetched = store.get_by_agent("agt_test_001").await;
    assert!(fetched.is_some(), "Should find conversation by agent");
    assert_eq!(fetched.unwrap().id, conv.id);

    // 7. A key's live scope by id: what a ChatKit client secret re-checks
    // on every use, so revoking the key ends its secrets at once.
    use omni2api_server::model::identity::KeyScope;
    let live = store.api_key_scope("key_test_001").await;
    assert!(live.is_some_and(|s| s.may_call()), "{live:?}");
    assert_eq!(store.api_key_scope("key_missing").await, None);
    let mut revoked = ApiKey::new(
        "key_test_001",
        "ohk_test_key",
        "agt_test_001",
        "hum_test_001",
    );
    revoked.scope = KeyScope::Revoked;
    store.insert_api_key(revoked).await;
    assert_eq!(
        store.api_key_scope("key_test_001").await,
        Some(KeyScope::Revoked)
    );

    clean_db(&store).await;
}
