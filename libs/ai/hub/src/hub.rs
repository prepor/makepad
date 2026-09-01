//! The hub object every app constructs: ephemeral AI compute and workload
//! management, in-process (aicore.md §0).
//!
//! Laws, enforced by construction:
//! - **Loopback-only, no announce**: an in-process hub opens NO listener and
//!   emits NO beacon — there is simply no socket in this module. The LAN face
//!   belongs exclusively to dedicated nodes (the `apps/ai-hub` binary /
//!   machine node) and arrives with the P3 fabric work.
//! - **No asset-server linkage**: the hub is compute; publishing generated
//!   content is the app's job through `makepad-asset-client`. Nothing here
//!   may import asset types.
//! - **The transcript lives with the app**: a session handle streams events;
//!   the hub side holds only engine state (KV warmth). Killing the hub costs
//!   a re-prefill, never a conversation (aicore.md §7).

#[cfg(feature = "llm")]
use crate::local_llm::{LocalLlmConfig, LocalLlmSession, ToolSpec, WakeHook};
use crate::pipe::PipeId;

/// Configuration for a local-model chat session.
#[cfg(feature = "llm")]
pub struct ChatConfig {
    /// Engine limits + the GGUF to load.
    pub llm: LocalLlmConfig,
    /// The caller's system instructions.
    pub system_prompt: String,
    /// The caller's tool pack, declared to the model in its own template.
    pub tools: Vec<ToolSpec>,
    /// Wake-up for sleeping consumers (UI apps pass their platform signal).
    pub wake: Option<WakeHook>,
}

/// One in-process hub. Cheap to construct; owns no threads until a session
/// or pipe actually starts.
pub struct AiHub {
    _private: (),
}

impl AiHub {
    /// The default for every app: fully in-process, loopback-only by
    /// construction (no listener exists to bind anything else).
    pub fn in_process() -> Self {
        Self { _private: () }
    }

    /// Start a chat on the in-process local model. Loading happens on the
    /// engine's worker thread and reports through the returned session's
    /// `poll()`; nothing blocks.
    #[cfg(feature = "llm")]
    pub fn start_local_chat(&self, config: ChatConfig) -> LocalLlmSession {
        let prefix = crate::local_llm::build_prefix(&config.system_prompt, &config.tools);
        LocalLlmSession::start(config.llm, prefix, config.wake)
    }

    /// The pipe id the in-process local model publishes (machine-local only).
    pub fn local_llm_pipe() -> PipeId {
        PipeId::new("llm.local")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_constructs_without_any_io() {
        let _hub = AiHub::in_process();
        assert_eq!(AiHub::local_llm_pipe().domain(), "llm");
    }
}
