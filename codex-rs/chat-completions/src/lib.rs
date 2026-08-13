//! `codex-chat-completions` — pure Chat Completions wire adapter.
//!
//! This crate translates between the Responses-API types used throughout the
//! Codex Rust workspace and the OpenAI Chat Completions wire format.  It
//! handles:
//!
//! - Building a Chat Completions JSON request body from a
//!   [`ResponsesApiRequest`].
//! - Parsing a Chat-Completions SSE byte stream into a stream of
//!   [`ResponseEvent`] values.
//! - Encoding / decoding tool names that embed an optional namespace.
//!
//! **No HTTP, no auth, no transport** — this crate is deliberately thin.
//! The caller is responsible for making the actual HTTP request and providing
//! the resulting byte stream.

mod request;
mod response;
mod tool_map;

pub use crate::request::build_chat_request;
pub use crate::response::parse_chat_sse_stream;
pub use crate::tool_map::ToolKind;
pub use crate::tool_map::ToolReverseMap;
pub use crate::tool_map::decode_name;
pub use crate::tool_map::encode_name;
pub use crate::tool_map::translate_tools;
