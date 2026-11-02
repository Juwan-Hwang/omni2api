//! API Gateway — HTTP routes for OpenAI-compatible API + mock human endpoint.
//!
//! Phase 3: `POST /v1/chat/completions` (streaming + non-streaming)
//!          `POST /v1/mock/human-reply` (simulate human response)
//! Phase 10: `GET /v1/replies/{invocation_id}` (async reply polling)

pub mod adapters;
pub mod audio;
pub mod batches;
pub mod completions;
pub mod conversations;
pub mod embeddings;
pub mod files;
pub mod history;
pub mod images;
pub mod mock;
pub mod moderations;
pub mod openai;
pub mod replies;
pub mod stored;
pub mod task;
pub mod uploads;
pub mod vector_stores;
pub mod videos;
