//! Interactive user prompting tool for cross-channel confirmations.

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;

/// Shared handle giving tools late-bound access to the live channel map.
pub type ChannelMapHandle = Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>;

/// Default timeout in seconds when waiting for a user response.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Agent-callable tool for sending a question to a user and waiting for their response.
pub struct AskUserTool {
    security: Arc<SecurityPolicy>,
    channels: ChannelMapHandle,
}

impl AskUserTool {
    /// Create a new ask_user tool using the given channel map.
    pub fn new(security: Arc<SecurityPolicy>, channels: ChannelMapHandle) -> Self {
        Self { security, channels }
    }
}

/// Format a question with optional choices for display.
fn format_question(question: &str, choices: Option<&[String]>) -> String {
    let mut lines = Vec::new();
    lines.push(format!("**{question}**"));

    if let Some(choices) = choices {
        lines.push(String::new());
        for (i, choice) in choices.iter().enumerate() {
            lines.push(format!("{}. {choice}", i + 1));
        }
        lines.push(String::new());
        lines.push("_Reply with a number or type your answer._".to_string());
    }

    lines.join("\n")
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. \
         Sends the question to a messaging channel and blocks until the user replies \
         or the timeout expires. Optionally provide choices for structured responses.\n\
         \n\
         With `recipient` (explicit, or auto-filled from the current conversation on \
         channel turns) the question is delivered to that chat and the tool returns \
         immediately — do not wait for the answer; it arrives as the next inbound \
         message in that conversation. Without a recipient (CLI/ACP back-channel \
         turns) the tool blocks until the user replies or the timeout expires."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "choices": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of choices (renders as buttons on Telegram, numbered list on CLI)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Seconds to wait for a response (default: 300)"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel name. Defaults to the first available channel if omitted."
                },
                "recipient": {
                    "type": "string",
                    "description": "Chat address to deliver the question to (e.g. a WhatsApp chat JID \
                                    or phone number, a Telegram chat id). Defaults to the current \
                                    conversation's chat on channel turns. With a recipient the tool \
                                    delivers the question and returns immediately — the reply arrives \
                                    as the next inbound message, it does not block."
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security gate: Act operation
        if let Err(e) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "ask_user")
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Action blocked: {e}")),
            });
        }

        // Parse required params
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "question"})),
                    "ask_user: missing question parameter"
                );
                anyhow::Error::msg("Missing 'question' parameter")
            })?
            .to_string();

        let choices: Option<Vec<String>> = args.get("choices").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        });

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let requested_channel = args
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        // Explicit chat address. The turn engine also injects one from the
        // originating conversation (see `delivery_defaults`), so on channel
        // turns this is normally always present.
        let recipient: Option<String> = args
            .get("recipient")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        // Resolve channel from handle — block-scoped to drop the RwLock guard
        // before any `.await` (parking_lot guards are !Send).
        let (channel_name, channel): (String, Arc<dyn Channel>) = {
            let channels = self.channels.read();
            if channels.is_empty() {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("No channels available yet (channels not initialized)".to_string()),
                });
            }
            if let Some(ref name) = requested_channel {
                let ch = channels.get(name.as_str()).cloned().ok_or_else(|| {
                    let available = channels.keys().cloned().collect::<Vec<_>>().join(", ");
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "channel_requested": name,
                                "available": &available,
                            })),
                        "ask_user: requested channel not found"
                    );
                    anyhow::Error::msg(format!(
                        "Channel '{name}' not found. Available: {available}"
                    ))
                })?;
                (name.clone(), ch)
            } else {
                let (name, ch) = channels.iter().next().ok_or_else(|| {
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"missing": "channels"})),
                        "ask_user: no channels configured"
                    );
                    anyhow::Error::msg("No channels available. Configure at least one channel.")
                })?;
                (name.clone(), ch.clone())
            }
        };

        // Deliver-and-return: when the question targets a specific chat, its
        // reply arrives through the channel's normal inbound pipeline as a new
        // message. Spawning a second `listen` here would race the channel's
        // own listener — persistent-connection channels (whatsapp-web, telegram
        // getUpdates) have exactly one inbound pump, and a second one steals
        // updates or replaces the dispatch target — so the tool returns
        // immediately instead of blocking on `listen`.
        if let Some(recipient) = recipient.as_deref() {
            let text = format_question(&question, choices.as_deref());
            let msg = SendMessage::new(&text, recipient);
            if let Err(e) = channel.send(&msg).await {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "channel": &channel_name,
                            "recipient": recipient,
                            "error": format!("{}", e),
                        })),
                    "ask_user: delivering question to recipient failed"
                );
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!(
                        "Failed to send question to channel '{channel_name}' for recipient \
                         '{recipient}': {e}"
                    )),
                });
            }
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "channel": &channel_name,
                        "recipient": recipient,
                    })),
                "ask_user: question delivered to recipient"
            );
            return Ok(ToolResult {
                success: true,
                output: format!(
                    "Question delivered to '{recipient}' on channel '{channel_name}'. The reply \
                     will arrive as the next inbound message in that conversation — do not wait \
                     for it here; end your turn or continue with other work."
                )
                .into(),
                error: None,
            });
        }

        let timeout = std::time::Duration::from_secs(timeout_secs);

        // Prefer the channel's native structured-choice flow when choices are
        // present (e.g. ACP `session/request_permission` / `elicitation/create`,
        // Telegram inline keyboard).
        //
        // `Ok(None)` is overloaded on the Channel trait:
        //   - default impl / no native UI → "caller should fall back to send+listen"
        //   - ACP/RPC after Decline|Cancel → also `Ok(None)`
        // Only free-form-capable channels may take the send+listen fallback.
        // Structured-only channels (ACP, RPC Code tab, WS approval) must fail
        // fast: their `listen` never delivers a reply, so fallthrough either
        // hangs until timeout or returns a misleading "channel closed" error.
        if let Some(ref choices_vec) = choices
            && !choices_vec.is_empty()
        {
            match channel
                .request_choice(&question, choices_vec, timeout)
                .await
            {
                Ok(Some(answer)) => {
                    return Ok(ToolResult {
                        success: true,
                        output: answer.into(),
                        error: None,
                    });
                }
                Ok(None) if channel.supports_free_form_ask() => {
                    /* fall through to send+listen */
                }
                Ok(None) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "Channel '{channel_name}' did not complete the structured choice \
                             (user cancelled/declined, or the channel has no free-form fallback). \
                             Retry ask_user; free-form questions on this channel await ACP \
                             elicitation Phase 2."
                        )),
                    });
                }
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!(
                            "Failed to ask question on channel '{channel_name}': {e}"
                        )),
                    });
                }
            }
        } else if !channel.supports_free_form_ask() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Channel '{channel_name}' requires `choices` for ask_user \
                     (free-form questions await ACP elicitation Phase 2)"
                )),
            });
        }

        // Format and send the question
        let text = format_question(&question, choices.as_deref());
        let msg = SendMessage::new(&text, "");
        if let Err(e) = channel.send(&msg).await {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Failed to send question to channel '{channel_name}': {e}"
                )),
            });
        }

        // Listen for user response with timeout
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ChannelMessage>(1);

        // Spawn a listener task on the channel
        let listen_channel = Arc::clone(&channel);
        let listen_handle = zeroclaw_spawn::spawn!(async move { listen_channel.listen(tx).await });

        let response = tokio::time::timeout(timeout, rx.recv()).await;

        // Abort the listener once we have a response or timeout
        listen_handle.abort();

        match response {
            Ok(Some(msg)) => Ok(ToolResult {
                success: true,
                output: msg.content.into(),
                error: None,
            }),
            Ok(None) => Ok(ToolResult {
                success: false,
                output: "TIMEOUT".to_string().into(),
                error: Some("Channel closed before receiving a response".to_string()),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: "TIMEOUT".to_string().into(),
                error: Some(format!(
                    "No response received within {timeout_secs} seconds"
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub channel that records sent messages but never produces incoming messages.
    struct SilentChannel {
        channel_name: String,
        sent: Arc<RwLock<Vec<String>>>,
    }

    impl SilentChannel {
        fn new(name: &str) -> Self {
            Self {
                channel_name: name.to_string(),
                sent: Arc::new(RwLock::new(Vec::new())),
            }
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for SilentChannel {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Channel(
                ::zeroclaw_api::attribution::ChannelKind::Webhook,
            )
        }
        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl Channel for SilentChannel {
        fn name(&self) -> &str {
            &self.channel_name
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.write().push(message.content.clone());
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            // Never sends anything — simulates no user response
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            Ok(())
        }
    }

    /// A stub channel that immediately responds with a canned message.
    struct RespondingChannel {
        channel_name: String,
        response: String,
        sent: Arc<RwLock<Vec<String>>>,
    }

    impl RespondingChannel {
        fn new(name: &str, response: &str) -> Self {
            Self {
                channel_name: name.to_string(),
                response: response.to_string(),
                sent: Arc::new(RwLock::new(Vec::new())),
            }
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for RespondingChannel {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Channel(
                ::zeroclaw_api::attribution::ChannelKind::Webhook,
            )
        }
        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl Channel for RespondingChannel {
        fn name(&self) -> &str {
            &self.channel_name
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent.write().push(message.content.clone());
            Ok(())
        }

        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            let msg = ChannelMessage {
                id: "resp_1".to_string(),
                sender: "user".to_string(),
                reply_target: "user".to_string(),
                content: self.response.clone(),
                channel: self.channel_name.clone(),
                channel_alias: None,
                timestamp: 1000,
                thread_ts: None,
                interruption_scope_id: None,
                attachments: vec![],
                subject: None,

                ..Default::default()
            };
            let _ = tx.send(msg).await;
            Ok(())
        }
    }

    fn make_tool_with_channels(channels: Vec<(&str, Arc<dyn Channel>)>) -> AskUserTool {
        let handle = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut map = handle.write();
            for (name, ch) in channels {
                map.insert(name.to_string(), ch);
            }
        }
        AskUserTool::new(Arc::new(SecurityPolicy::default()), handle)
    }

    // ── Metadata tests ──

    #[test]
    fn tool_name_and_description() {
        let tool = AskUserTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        assert_eq!(tool.name(), "ask_user");
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("question"));
    }

    #[test]
    fn parameter_schema_validation() {
        let tool = AskUserTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["question"].is_object());
        assert!(schema["properties"]["choices"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
        assert!(schema["properties"]["channel"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "question"));
        // choices, timeout_secs, channel are optional
        assert!(!required.iter().any(|v| v == "choices"));
        assert!(!required.iter().any(|v| v == "timeout_secs"));
        assert!(!required.iter().any(|v| v == "channel"));
    }

    #[test]
    fn spec_matches_metadata() {
        let tool = AskUserTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        let spec = tool.spec();
        assert_eq!(spec.name, "ask_user");
        assert_eq!(spec.description, tool.description());
        assert!(spec.parameters["required"].is_array());
    }

    // ── Format question tests ──

    #[test]
    fn format_question_without_choices() {
        let text = format_question("Are you sure?", None);
        assert!(text.contains("Are you sure?"));
        assert!(!text.contains("1."));
    }

    #[test]
    fn format_question_with_choices() {
        let choices = vec!["Yes".to_string(), "No".to_string(), "Maybe".to_string()];
        let text = format_question("Continue?", Some(&choices));
        assert!(text.contains("Continue?"));
        assert!(text.contains("1. Yes"));
        assert!(text.contains("2. No"));
        assert!(text.contains("3. Maybe"));
        assert!(text.contains("Reply with a number"));
    }

    // ── Execute tests ──

    #[tokio::test]
    async fn execute_rejects_missing_question() {
        let tool = make_tool_with_channels(vec![(
            "test",
            Arc::new(SilentChannel::new("test")) as Arc<dyn Channel>,
        )]);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_rejects_empty_question() {
        let tool = make_tool_with_channels(vec![(
            "test",
            Arc::new(SilentChannel::new("test")) as Arc<dyn Channel>,
        )]);
        let result = tool.execute(json!({ "question": "  " })).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_channels_returns_not_initialized() {
        let tool = AskUserTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        let result = tool.execute(json!({ "question": "Hello?" })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("not initialized"));
    }

    #[tokio::test]
    async fn unknown_channel_returns_error() {
        let tool = make_tool_with_channels(vec![(
            "slack",
            Arc::new(SilentChannel::new("slack")) as Arc<dyn Channel>,
        )]);
        let result = tool
            .execute(json!({ "question": "Hello?", "channel": "nonexistent" }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn timeout_returns_timeout_output() {
        let tool = make_tool_with_channels(vec![(
            "test",
            Arc::new(SilentChannel::new("test")) as Arc<dyn Channel>,
        )]);
        let result = tool
            .execute(json!({
                "question": "Confirm?",
                "timeout_secs": 1
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.output, "TIMEOUT");
        assert!(result.error.as_deref().unwrap().contains("1 seconds"));
    }

    #[tokio::test]
    async fn successful_response_flow() {
        let tool = make_tool_with_channels(vec![(
            "test",
            Arc::new(RespondingChannel::new("test", "Yes, proceed!")) as Arc<dyn Channel>,
        )]);
        let result = tool
            .execute(json!({
                "question": "Should we deploy?",
                "timeout_secs": 5
            }))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert_eq!(result.output, "Yes, proceed!");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn successful_response_with_choices() {
        let tool = make_tool_with_channels(vec![(
            "telegram",
            Arc::new(RespondingChannel::new("telegram", "2")) as Arc<dyn Channel>,
        )]);
        let result = tool
            .execute(json!({
                "question": "Pick an option",
                "choices": ["Option A", "Option B"],
                "channel": "telegram",
                "timeout_secs": 5
            }))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert_eq!(result.output, "2");
    }

    #[tokio::test]
    async fn channel_map_handle_allows_late_binding() {
        let handle = Arc::new(RwLock::new(HashMap::new()));
        let tool = AskUserTool::new(Arc::new(SecurityPolicy::default()), handle.clone());

        // Initially empty — tool reports not initialized
        let result = tool.execute(json!({ "question": "Hello?" })).await.unwrap();
        assert!(!result.success);

        // Populate via the shared handle
        {
            let mut map = handle.write();
            map.insert(
                "cli".to_string(),
                Arc::new(RespondingChannel::new("cli", "ok")) as Arc<dyn Channel>,
            );
        }

        // Now the tool can route to the channel
        let result = tool
            .execute(json!({ "question": "Hello?", "timeout_secs": 5 }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "ok");
    }

    /// ACP/RPC-shaped channel: structured `request_choice` returns `Ok(None)`
    /// (user cancelled/declined, or no form capability), free-form is unsupported,
    /// and `listen` exits immediately without delivering a message. Falling
    /// through to send+listen would hang until `timeout_secs`.
    struct StructuredOnlyChannel {
        channel_name: String,
        choice_result: parking_lot::Mutex<Option<Result<Option<String>, String>>>,
        listen_called: parking_lot::Mutex<bool>,
    }

    impl StructuredOnlyChannel {
        fn new(name: &str, choice_result: Result<Option<String>, String>) -> Self {
            Self {
                channel_name: name.to_string(),
                choice_result: parking_lot::Mutex::new(Some(choice_result)),
                listen_called: parking_lot::Mutex::new(false),
            }
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for StructuredOnlyChannel {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Channel(
                ::zeroclaw_api::attribution::ChannelKind::AcpChannel,
            )
        }
        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl Channel for StructuredOnlyChannel {
        fn name(&self) -> &str {
            &self.channel_name
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            *self.listen_called.lock() = true;
            // Mirrors RpcApprovalChannel / AcpChannel: listen ends without a
            // message. If ask_user falls through here, rx.recv() hangs.
            Ok(())
        }

        fn supports_free_form_ask(&self) -> bool {
            false
        }

        async fn request_choice(
            &self,
            _question: &str,
            _choices: &[String],
            _timeout: std::time::Duration,
        ) -> anyhow::Result<Option<String>> {
            match self.choice_result.lock().take() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(anyhow::Error::msg(e)),
                None => Ok(None),
            }
        }
    }

    #[tokio::test]
    async fn request_choice_none_on_structured_only_channel_fails_fast() {
        // Regression: Ok(None) from request_choice used to fall through to
        // send+listen. On ACP/RPC channels listen never delivers a reply, so
        // the tool hung until timeout (default 300s) and the session had to
        // be cancelled manually.
        let ch = Arc::new(StructuredOnlyChannel::new("rpc", Ok(None)));
        let tool = make_tool_with_channels(vec![("rpc", Arc::clone(&ch) as Arc<dyn Channel>)]);

        let started = std::time::Instant::now();
        let result = tool
            .execute(json!({
                "question": "How should users set a name?",
                "choices": ["Slash only", "Slash + picker", "Something else"],
                "timeout_secs": 30
            }))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must fail fast, not wait on listen; elapsed={elapsed:?}"
        );
        assert!(
            !result.success,
            "expected failure, got success: {:?}",
            result.output
        );
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("rpc")
                || err.contains("choices")
                || err.contains("free-form")
                || err.contains("declined")
                || err.contains("cancelled")
                || err.contains("cancel"),
            "error should explain structured-only failure; got: {err}"
        );
        assert!(
            !*ch.listen_called.lock(),
            "must not fall through to listen on a structured-only channel"
        );
    }

    /// Records every send as (recipient, content) and marks any `listen`
    /// call. Persistent-connection channels (whatsapp-web, telegram) have a
    /// single inbound pump: a second `listen` would steal updates or replace
    /// the dispatch target, so a recipient-targeted ask must never listen.
    struct RecordingChannel {
        channel_name: String,
        sent: Arc<RwLock<Vec<(String, String)>>>,
        listen_called: Arc<std::sync::atomic::AtomicBool>,
    }

    /// Shared handles a test reads back after `execute` returns.
    struct RecordingHandles {
        sent: Arc<RwLock<Vec<(String, String)>>>,
        listen_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl RecordingChannel {
        fn new(name: &str) -> (Self, RecordingHandles) {
            let sent = Arc::new(RwLock::new(Vec::new()));
            let listen_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Self {
                    channel_name: name.to_string(),
                    sent: Arc::clone(&sent),
                    listen_called: Arc::clone(&listen_called),
                },
                RecordingHandles {
                    sent,
                    listen_called,
                },
            )
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for RecordingChannel {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Channel(
                ::zeroclaw_api::attribution::ChannelKind::Webhook,
            )
        }
        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            &self.channel_name
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent
                .write()
                .push((message.recipient.clone(), message.content.clone()));
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        ) -> anyhow::Result<()> {
            self.listen_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn explicit_recipient_delivers_without_listen() {
        let (ch, handles) = RecordingChannel::new("whatsapp");
        let tool = make_tool_with_channels(vec![("whatsapp", Arc::new(ch) as Arc<dyn Channel>)]);

        let result = tool
            .execute(json!({
                "question": "Are you skipping dinner tonight?",
                "recipient": "+6593826756@s.whatsapp.net",
                "timeout_secs": 5
            }))
            .await
            .unwrap();

        assert!(result.success, "error: {:?}", result.error);
        let sent = handles.sent.read();
        assert_eq!(sent.len(), 1, "exactly one send");
        assert_eq!(sent[0].0, "+6593826756@s.whatsapp.net");
        assert!(sent[0].1.contains("Are you skipping dinner tonight?"));
        assert!(
            !handles
                .listen_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "a recipient-targeted ask must not spawn a second listen"
        );
        let output = result.output.to_string();
        assert!(
            output.contains("delivered") && output.contains("+6593826756@s.whatsapp.net"),
            "output must tell the model the question landed; got: {output}"
        );
    }

    #[tokio::test]
    async fn explicit_recipient_with_choices_skips_native_choice_flow() {
        // With choices, the pre-recipient path called request_choice (native
        // UI for the CURRENT session) — wrong when the question targets a
        // specific chat. The recipient path must send the numbered list as
        // text and return.
        let (ch, handles) = RecordingChannel::new("whatsapp");
        let tool = make_tool_with_channels(vec![("whatsapp", Arc::new(ch) as Arc<dyn Channel>)]);

        let result = tool
            .execute(json!({
                "question": "Dinner at home or out?",
                "choices": ["Home", "Out"],
                "recipient": "+6593826756@s.whatsapp.net"
            }))
            .await
            .unwrap();

        assert!(result.success, "error: {:?}", result.error);
        let sent = handles.sent.read();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].1.contains("1. Home"),
            "choices rendered: {}",
            sent[0].1
        );
        assert!(sent[0].1.contains("Reply with a number"));
        assert!(
            !handles
                .listen_called
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn recipient_send_failure_reports_error() {
        struct FailingSendChannel {
            channel_name: String,
        }
        impl ::zeroclaw_api::attribution::Attributable for FailingSendChannel {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Channel(
                    ::zeroclaw_api::attribution::ChannelKind::Webhook,
                )
            }
            fn alias(&self) -> &str {
                "test"
            }
        }
        #[async_trait]
        impl Channel for FailingSendChannel {
            fn name(&self) -> &str {
                &self.channel_name
            }
            async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
                anyhow::bail!("delivery refused")
            }
            async fn listen(
                &self,
                _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
            ) -> anyhow::Result<()> {
                anyhow::bail!("listen must not be called")
            }
        }

        let tool = make_tool_with_channels(vec![(
            "whatsapp",
            Arc::new(FailingSendChannel {
                channel_name: "whatsapp".to_string(),
            }) as Arc<dyn Channel>,
        )]);
        let result = tool
            .execute(json!({
                "question": "Hello?",
                "recipient": "+6593826756@s.whatsapp.net"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("whatsapp") && err.contains("delivery refused"),
            "error should name channel and cause; got: {err}"
        );
    }
}
