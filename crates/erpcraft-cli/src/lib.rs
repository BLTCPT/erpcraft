//! ERPCraft's agent/operator surface: one dispatch table
//! ([`commands`]), two doors — the `erpcraft` CLI binary and the
//! `erpcraft mcp` stdio server ([`mcp`]). Both doors route into the
//! same dispatch functions and from there into `erpcraft-core`
//! directly — never HTTP, never re-implemented posting logic (the
//! `bt mcp` precedent in the rust-backtester).

pub mod agent;
pub mod commands;
pub mod mcp;
