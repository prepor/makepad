//! The local model, on a thread of its own.
//!
//! A `makepad_ai_llm::LlamaSession` (Qwen3.5-9B on makepad-ggml — pure Rust,
//! no external process, nothing over the network) running the same harness the
//! route app's dispatcher uses. The session is `!Send`, so it is built on, and
//! never leaves, one dedicated worker thread; the UI talks to it over a pair of
//! channels and is woken with `SignalToUI`.
//!
//! The session is append-only across turns. `reset()` would reload every weight
//! from disk, and appending means the system-and-tools prefix and the whole
//! conversation stay in the KV cache: each turn only prefills its own suffix.
//!
//! Tool calls follow Qwen3.5's own chat template: a leading system message
//! declares `<tools>` (one JSON schema per line) and the model answers with
//! `<tool_call>\n<function=name>\n<parameter=key>\nvalue\n</parameter>…`. Tool
//! results go back as a user turn of `<tool_response>` blocks. Nothing here
//! parses JSON — the wire between the parser and the tools is a list of
//! (key, value) strings, because both ends of it live in this crate.

use makepad_ai_llm::{LlamaSession, LlamaSessionConfig};
use makepad_widgets::makepad_platform::thread::SignalToUI;

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    thread,
};

/// Where the weights live, relative to the checkout this was built from.
pub const MODEL_FILE: &str = "local/models/Qwen3.5-9B-UD-Q4_K_XL.gguf";
/// The environment variable that overrides it.
pub const MODEL_ENV: &str = "MPFILES_CHAT_MODEL";

/// The session is append-only — the tools prefix plus every turn accumulates —
/// so the window is the conversation's whole life. Only 12 of the hybrid
/// model's 48 layers are attention, so this costs well under a gigabyte.
const MAX_CONTEXT: u32 = 16384;
/// A file question is not an essay. Past this the answer has stopped being one.
const MAX_NEW_TOKENS: usize = 640;
/// Stop a turn while this much context is still left, so the next one fits.
const MIN_REMAINING_CONTEXT: usize = 256;

/// One tool, as the model is told about it.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// A JSON-schema object, verbatim.
    pub parameters: &'static str,
}

/// What the worker has to say. Everything the panel shows comes from here.
#[derive(Debug)]
pub enum ChatEvent {
    /// A named phase of the load, and how far through it is (0..1).
    Loading { phase: String, fraction: f64 },
    /// The weights are resident and the prefix is prefilled.
    Ready { prefill_tokens: usize, secs: f64 },
    /// The model could not be loaded at all. Nothing else will ever arrive.
    Failed(String),
    /// A piece of the answer being written.
    Delta(String),
    /// The model wants a tool run. The app owes exactly one result per call.
    ToolCall { name: String, args: Vec<(String, String)> },
    /// The turn is over. `tool_calls` is how many results the app now owes.
    TurnDone {
        tool_calls: usize,
        tokens: usize,
        secs: f64,
        context_used: usize,
        context_max: usize,
    },
    /// The window is full; this session cannot continue.
    ContextFull,
}

enum WorkerMsg {
    UserTurn(String),
    /// One (text, is_error) per tool the last turn asked for, in call order.
    ToolResults(Vec<(String, bool)>),
}

pub struct ChatAgent {
    to_worker: Sender<WorkerMsg>,
    from_worker: Receiver<ChatEvent>,
    cancel: Arc<AtomicBool>,
}

impl ChatAgent {
    /// Start loading the model. Nothing blocks: the load happens on the worker
    /// and reports itself through [`ChatAgent::poll`].
    pub fn start(model: &Path, prefix: String) -> Self {
        let (event_tx, from_worker) = channel();
        let (to_worker, msg_rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let model = model.to_path_buf();
        thread::spawn(move || worker_main(model, prefix, msg_rx, event_tx, worker_cancel));
        Self {
            to_worker,
            from_worker,
            cancel,
        }
    }

    pub fn send_user_turn(&self, text: String) {
        self.cancel.store(false, Ordering::Relaxed);
        let _ = self.to_worker.send(WorkerMsg::UserTurn(text));
    }

    pub fn send_tool_results(&self, results: Vec<(String, bool)>) {
        let _ = self.to_worker.send(WorkerMsg::ToolResults(results));
    }

    /// Stop the turn that is running, per token. The worker closes the dangling
    /// assistant turn so the cache stays valid and drops any tool calls it had
    /// collected — an interrupted question must not keep acting.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn poll(&self) -> Vec<ChatEvent> {
        self.from_worker.try_iter().collect()
    }
}

/// Where the weights are, or `None` when this machine has none.
///
/// `MPFILES_CHAT_MODEL` wins; otherwise the file is looked for relative to the
/// working directory, then up from the binary (which finds `target/release`
/// runs from anywhere), then in the checkout this binary was compiled in.
pub fn model_path() -> Option<PathBuf> {
    if let Some(from_env) = std::env::var_os(MODEL_ENV) {
        let path = PathBuf::from(from_env);
        return path.is_file().then_some(path);
    }
    let relative = Path::new(MODEL_FILE);
    if relative.is_file() {
        return Some(relative.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        for base in exe.ancestors() {
            let candidate = base.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join(relative))?;
    checkout.is_file().then_some(checkout)
}

// --------------------------------------------------------------- the prompt

/// The system turn: the tools block in Qwen3.5's own template, then the app's
/// own instructions, ending ready for the first user turn.
pub fn build_prefix(system_prompt: &str, tools: &[ToolSpec]) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<|im_start|>system\n");
    if !tools.is_empty() {
        out.push_str("# Tools\n\nYou have access to the following functions:\n\n<tools>\n");
        for tool in tools {
            out.push_str("{\"name\":");
            push_json_string(&mut out, tool.name);
            out.push_str(",\"description\":");
            push_json_string(&mut out, tool.description);
            out.push_str(",\"parameters\":");
            out.push_str(tool.parameters);
            out.push_str("}\n");
        }
        out.push_str("</tools>\n\n");
        out.push_str(
            "If you choose to call a function ONLY reply in the following format with NO suffix:\n\n\
             <tool_call>\n<function=example_function_name>\n<parameter=example_parameter_1>\n\
             value_1\n</parameter>\n</function>\n</tool_call>\n\n\
             <IMPORTANT>\nReminder:\n\
             - Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n\
             - Required parameters MUST be specified\n\
             - You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n\
             - If there is no function call available, answer the question like normal\n\
             </IMPORTANT>\n\n",
        );
    }
    out.push_str(system_prompt);
    out.push_str("<|im_end|>\n");
    out
}

/// A JSON string literal, appended. The only JSON this module writes.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn user_turn(text: &str) -> String {
    format!("<|im_start|>user\n{text}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n")
}

fn tool_response_turn(results: &[(String, bool)]) -> String {
    let mut out = String::from("<|im_start|>user\n");
    for (result, is_error) in results {
        out.push_str("<tool_response>\n");
        if *is_error {
            out.push_str("ERROR: ");
        }
        out.push_str(result);
        out.push_str("\n</tool_response>\n");
    }
    out.push_str("<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n");
    out
}

// --------------------------------------------------------------- the worker

fn worker_main(
    model: PathBuf,
    prefix: String,
    msg_rx: Receiver<WorkerMsg>,
    event_tx: Sender<ChatEvent>,
    cancel: Arc<AtomicBool>,
) {
    let send = |event: ChatEvent| {
        let _ = event_tx.send(event);
        SignalToUI::set_ui_signal();
    };

    let started = std::time::Instant::now();
    let load = {
        let event_tx = event_tx.clone();
        let mut progress = move |phase: &str, fraction: f64| {
            let _ = event_tx.send(ChatEvent::Loading {
                phase: phase.to_string(),
                fraction,
            });
            SignalToUI::set_ui_signal();
        };
        LlamaSession::load_with_progress(
            &model,
            LlamaSessionConfig {
                max_context: Some(MAX_CONTEXT),
                ..Default::default()
            },
            &mut progress,
        )
    };
    let mut session = match load {
        Ok(session) => session,
        Err(error) => {
            send(ChatEvent::Failed(format!(
                "could not load {}: {error:?}",
                model.display()
            )));
            return;
        }
    };

    let tokens = match session.vocab().tokenize(&prefix, true, true) {
        Ok(tokens) => tokens,
        Err(error) => return send(ChatEvent::Failed(format!("tokenize: {error:?}"))),
    };
    let prefill_tokens = tokens.len();
    if let Err(error) = session.append_tokens(&tokens) {
        return send(ChatEvent::Failed(format!("prefill: {error:?}")));
    }
    send(ChatEvent::Ready {
        prefill_tokens,
        secs: started.elapsed().as_secs_f64(),
    });

    let im_end = session.vocab().token_id("<|im_end|>");
    let tool_call_open = session.vocab().token_id("<tool_call>");
    let tool_call_close = session.vocab().token_id("</tool_call>");
    let think_open = session.vocab().token_id("<think>");
    let think_close = session.vocab().token_id("</think>");

    while let Ok(msg) = msg_rx.recv() {
        let turn_text = match msg {
            WorkerMsg::UserTurn(text) => user_turn(&text),
            WorkerMsg::ToolResults(results) => tool_response_turn(&results),
        };
        let turn_tokens = match session.vocab().tokenize(&turn_text, true, true) {
            Ok(tokens) => tokens,
            Err(_) => continue,
        };
        if session.remaining_context() < turn_tokens.len() + MIN_REMAINING_CONTEXT
            || session.append_tokens(&turn_tokens).is_err()
        {
            send(ChatEvent::ContextFull);
            continue;
        }

        // Stream the answer; capture <tool_call> bodies; swallow <think>.
        let generating = std::time::Instant::now();
        let mut decoder = session.vocab().text_decoder();
        let mut generated = 0usize;
        let mut in_tool_call = false;
        let mut in_think = false;
        let mut tool_body = String::new();
        let mut tool_calls: Vec<(String, Vec<(String, String)>)> = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                // Close the dangling assistant turn so the cache stays valid,
                // and drop the tool calls: an interrupted question must not
                // keep looking at things.
                if let Some(im_end) = im_end {
                    let _ = session.append_token(im_end);
                }
                tool_calls.clear();
                break;
            }
            if generated >= MAX_NEW_TOKENS || session.remaining_context() < MIN_REMAINING_CONTEXT {
                if let Some(im_end) = im_end {
                    let _ = session.append_token(im_end);
                }
                break;
            }
            let token = match session.next_greedy_token() {
                Ok(Some(token)) => token,
                // End of turn, or nothing left to say.
                _ => break,
            };
            generated += 1;
            if Some(token) == tool_call_open {
                in_tool_call = true;
                tool_body.clear();
                continue;
            }
            if Some(token) == tool_call_close {
                if in_tool_call {
                    in_tool_call = false;
                    match parse_tool_call(&tool_body) {
                        Ok(call) => tool_calls.push(call),
                        // Malformed: show what it tried rather than hanging.
                        Err(error) => send(ChatEvent::Delta(format!("[bad tool call: {error}]"))),
                    }
                }
                continue;
            }
            if Some(token) == think_open {
                in_think = true;
                continue;
            }
            if Some(token) == think_close {
                in_think = false;
                continue;
            }
            if let Some(text) = decoder.push_token(session.vocab(), token) {
                if in_tool_call {
                    tool_body.push_str(&text);
                } else if !in_think {
                    send(ChatEvent::Delta(text));
                }
            }
        }
        let secs = generating.elapsed().as_secs_f64();
        let tool_call_count = tool_calls.len();
        for (name, args) in tool_calls {
            send(ChatEvent::ToolCall { name, args });
        }
        send(ChatEvent::TurnDone {
            tool_calls: tool_call_count,
            tokens: generated,
            secs,
            context_used: session.token_count(),
            context_max: session.max_context(),
        });
    }
}

// --------------------------------------------------------- tool-call parsing

/// The body between `<tool_call>` and `</tool_call>`: `<function=NAME>` and a
/// run of `<parameter=key>\nvalue\n</parameter>`, into a name and its
/// arguments. Values stay strings — the tools that read them know what they
/// want, and a number that arrived as text is still a number.
fn parse_tool_call(body: &str) -> Result<(String, Vec<(String, String)>), String> {
    let function_at = body.find("<function=").ok_or("missing <function=")?;
    let rest = &body[function_at + "<function=".len()..];
    let name_end = rest.find(['>', '\n']).ok_or("unterminated function name")?;
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return Err("empty function name".into());
    }
    let mut args = Vec::new();
    let mut cursor = &rest[name_end..];
    while let Some(param_at) = cursor.find("<parameter=") {
        let param_rest = &cursor[param_at + "<parameter=".len()..];
        let Some(key_end) = param_rest.find('>') else {
            break;
        };
        let key = param_rest[..key_end].trim().to_string();
        let value_rest = &param_rest[key_end + 1..];
        let value_end = value_rest.find("</parameter>").unwrap_or(value_rest.len());
        let value = value_rest[..value_end].trim_matches('\n').trim().to_string();
        args.push((key, value));
        cursor = &value_rest[value_end..];
    }
    Ok((name, args))
}

/// One argument by name, or the empty string.
pub fn arg<'a>(args: &'a [(String, String)], key: &str) -> &'a str {
    args.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_call() {
        let body = "\n<function=list_dir>\n<parameter=path>\n~/Documents\n</parameter>\n</function>\n";
        let (name, args) = parse_tool_call(body).unwrap();
        assert_eq!(name, "list_dir");
        assert_eq!(arg(&args, "path"), "~/Documents");
        assert_eq!(arg(&args, "missing"), "");
    }

    #[test]
    fn parses_several_parameters_in_order() {
        let body = "<function=read_file>\n<parameter=path>\n/tmp/a.txt\n</parameter>\n\
                    <parameter=max_bytes>\n4096\n</parameter>\n</function>";
        let (name, args) = parse_tool_call(body).unwrap();
        assert_eq!(name, "read_file");
        assert_eq!(
            args,
            vec![
                ("path".to_string(), "/tmp/a.txt".to_string()),
                ("max_bytes".to_string(), "4096".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_a_body_with_no_function() {
        assert!(parse_tool_call("just some prose").is_err());
    }

    #[test]
    fn the_prefix_carries_the_tools_block() {
        let tools = [ToolSpec {
            name: "list_dir",
            description: "List a folder's entries.",
            parameters: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
        }];
        let prefix = build_prefix("You are a file assistant.", &tools);
        assert!(prefix.starts_with("<|im_start|>system\n# Tools"));
        assert!(prefix.contains("\"name\":\"list_dir\""));
        assert!(prefix.contains("<tools>\n"));
        assert!(prefix.ends_with("You are a file assistant.<|im_end|>\n"));
    }

    #[test]
    fn json_strings_are_escaped() {
        let mut out = String::new();
        push_json_string(&mut out, "a \"quoted\" \\ path\nnewline");
        assert_eq!(out, "\"a \\\"quoted\\\" \\\\ path\\nnewline\"");
    }
}
