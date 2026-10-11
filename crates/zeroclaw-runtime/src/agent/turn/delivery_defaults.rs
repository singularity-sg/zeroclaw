//! Channel delivery-default injection for cron_add and interactive tools.

const AUTO_DELIVERY_DEFAULT_CHANNELS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "mattermost",
    "matrix",
    "dingtalk",
    "lark",
    "feishu",
];

/// Tools that take an optional `channel` arg and otherwise pick
/// `channels.iter().next()` (HashMap order). Without an originating-channel
/// default they can route an interactive prompt to a random external channel
/// while the user is sitting in the RPC/ACP/WS session that issued the turn —
/// which looks like a hang until timeout/cancel.
const ORIGINATING_CHANNEL_TOOLS: &[&str] = &["ask_user", "poll", "escalate_to_human"];

/// When `tool_args` omits `channel`, fill it from the turn's originating
/// channel name so interactive tools prefer the conversation back-channel
/// (`rpc` / `acp` / `ws` / …) over an arbitrary map entry.
pub(crate) fn maybe_inject_originating_channel(
    tool_name: &str,
    tool_args: &mut serde_json::Value,
    channel_name: &str,
) {
    if !ORIGINATING_CHANNEL_TOOLS.contains(&tool_name) {
        return;
    }
    let channel_name = channel_name.trim();
    if channel_name.is_empty() || channel_name == "agent" || channel_name == "daemon" {
        // Configless / non-channel turns have no conversation back-channel to
        // prefer; leave selection to the tool's existing map fallback.
        return;
    }
    let Some(args) = tool_args.as_object_mut() else {
        return;
    };
    let needs_channel = args
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|value| value.trim().is_empty());
    if needs_channel {
        args.insert(
            "channel".to_string(),
            serde_json::Value::String(channel_name.to_string()),
        );
    }
}

/// ask_user sends its question with an empty recipient unless one is given,
/// and on persistent-connection channels an empty recipient delivers nowhere
/// (their `send` is a real chat delivery, not a CLI/ACP back-channel write).
/// When the turn carries a reply target — the agent is mid-conversation with
/// a real chat — default the question to that chat. An explicit `recipient`
/// in the call wins; turns without a reply target (CLI, ACP, RPC) keep the
/// blocking back-channel behavior.
fn maybe_inject_ask_user_recipient(
    tool_name: &str,
    tool_args: &mut serde_json::Value,
    channel_reply_target: Option<&str>,
) {
    if tool_name != "ask_user" {
        return;
    }
    let Some(reply_target) = channel_reply_target
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(args) = tool_args.as_object_mut() else {
        return;
    };
    let needs_recipient = args
        .get("recipient")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|value| value.trim().is_empty());
    if needs_recipient {
        args.insert(
            "recipient".to_string(),
            serde_json::Value::String(reply_target.to_string()),
        );
    }
}

pub(crate) fn maybe_inject_channel_delivery_defaults(
    tool_name: &str,
    tool_args: &mut serde_json::Value,
    channel_name: &str,
    channel_alias: Option<&str>,
    channel_reply_target: Option<&str>,
) {
    // Interactive tools first — independent of cron delivery defaults.
    maybe_inject_originating_channel(tool_name, tool_args, channel_name);
    maybe_inject_ask_user_recipient(tool_name, tool_args, channel_reply_target);

    if tool_name != "cron_add" {
        return;
    }

    if !AUTO_DELIVERY_DEFAULT_CHANNELS.contains(&channel_name) {
        return;
    }

    let Some(reply_target) = channel_reply_target
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let Some(args) = tool_args.as_object_mut() else {
        return;
    };

    let is_agent_job = args
        .get("job_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|job_type| job_type.eq_ignore_ascii_case("agent"))
        || args
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|prompt| !prompt.trim().is_empty());
    if !is_agent_job {
        return;
    }

    // Delivery resolves `<type>.<alias>`; the bare type is registered only
    // while that type has one configured instance (`configured_channel_map`).
    let delivery_channel = match channel_alias
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
    {
        Some(alias) => format!("{channel_name}.{alias}"),
        None => channel_name.to_string(),
    };

    let default_delivery = || {
        serde_json::json!({
            "mode": "announce",
            "channel": delivery_channel,
            "to": reply_target,
        })
    };

    match args.get_mut("delivery") {
        None => {
            args.insert("delivery".to_string(), default_delivery());
        }
        Some(delivery) if delivery.is_null() => {
            *delivery = default_delivery();
        }
        Some(serde_json::Value::Object(delivery)) => {
            if delivery
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|mode| mode.eq_ignore_ascii_case("none"))
            {
                return;
            }

            delivery
                .entry("mode".to_string())
                .or_insert_with(|| serde_json::Value::String("announce".to_string()));

            // Unset, or the bare type the turn-context block recommends: both
            // name this turn's instance. Anything else is a chosen destination.
            let needs_resolved_channel = delivery
                .get("channel")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|value| {
                    let value = value.trim();
                    value.is_empty() || value.eq_ignore_ascii_case(channel_name)
                });
            if needs_resolved_channel {
                delivery.insert(
                    "channel".to_string(),
                    serde_json::Value::String(delivery_channel.clone()),
                );
            }

            let needs_target = delivery
                .get("to")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|value| value.trim().is_empty());
            if needs_target {
                delivery.insert(
                    "to".to_string(),
                    serde_json::Value::String(reply_target.to_string()),
                );
            }
        }
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{maybe_inject_channel_delivery_defaults, maybe_inject_originating_channel};

    #[test]
    fn null_delivery_is_replaced_with_channel_defaults() {
        let mut args = serde_json::json!({
            "job_type": "agent",
            "delivery": null,
        });

        maybe_inject_channel_delivery_defaults(
            "cron_add",
            &mut args,
            "discord",
            None,
            Some("channel-42"),
        );

        assert_eq!(
            args["delivery"],
            serde_json::json!({
                "mode": "announce",
                "channel": "discord",
                "to": "channel-42",
            })
        );
    }

    #[test]
    fn absent_delivery_defaults_to_the_originating_composite_channel() {
        let mut args = serde_json::json!({
            "job_type": "agent",
            "prompt": "check the inbox",
        });

        maybe_inject_channel_delivery_defaults(
            "cron_add",
            &mut args,
            "telegram",
            Some("work"),
            Some("chat-42"),
        );

        assert_eq!(
            args["delivery"],
            serde_json::json!({
                "mode": "announce",
                "channel": "telegram.work",
                "to": "chat-42",
            })
        );
    }

    #[test]
    fn omitted_channel_key_is_filled_with_the_composite_channel() {
        // Observed shape: the model emits mode and to, and omits channel.
        let mut args = serde_json::json!({
            "job_type": "agent",
            "delivery": { "mode": "announce", "to": "chat-42" },
        });

        maybe_inject_channel_delivery_defaults(
            "cron_add",
            &mut args,
            "telegram",
            Some("work"),
            Some("chat-42"),
        );

        assert_eq!(args["delivery"]["channel"], "telegram.work");
    }

    #[test]
    fn an_explicit_destination_is_never_overridden() {
        // Includes this turn's own instance, which must not gain a second suffix.
        for chosen in ["telegram.work", "telegram.other", "discord", "discord.ops"] {
            let mut args = serde_json::json!({
                "job_type": "agent",
                "delivery": { "mode": "announce", "channel": chosen, "to": "chat-42" },
            });

            maybe_inject_channel_delivery_defaults(
                "cron_add",
                &mut args,
                "telegram",
                Some("work"),
                Some("chat-42"),
            );

            assert_eq!(args["delivery"]["channel"], chosen, "chosen={chosen}");
        }
    }

    #[test]
    fn bare_originating_type_is_upgraded_to_the_composite_channel() {
        // The turn-context block recommends this spelling to the model.
        for spelling in ["telegram", "TELEGRAM", "  telegram  "] {
            let mut args = serde_json::json!({
                "job_type": "agent",
                "delivery": { "mode": "announce", "channel": spelling, "to": "chat-42" },
            });

            maybe_inject_channel_delivery_defaults(
                "cron_add",
                &mut args,
                "telegram",
                Some("work"),
                Some("chat-42"),
            );

            assert_eq!(
                args["delivery"]["channel"], "telegram.work",
                "spelling={spelling}"
            );
        }
    }

    #[test]
    fn mode_none_still_suppresses_every_injection() {
        let mut args = serde_json::json!({
            "job_type": "agent",
            "delivery": { "mode": "none" },
        });

        maybe_inject_channel_delivery_defaults(
            "cron_add",
            &mut args,
            "telegram",
            Some("work"),
            Some("chat-42"),
        );

        assert_eq!(args["delivery"], serde_json::json!({ "mode": "none" }));
    }

    #[test]
    fn unaliased_channel_still_defaults_to_the_bare_type() {
        // CLI and daemon turns have no instance behind them.
        let mut args = serde_json::json!({
            "job_type": "agent",
            "prompt": "check the inbox",
        });

        maybe_inject_channel_delivery_defaults(
            "cron_add",
            &mut args,
            "telegram",
            Some("   "),
            Some("chat-42"),
        );

        assert_eq!(args["delivery"]["channel"], "telegram");
    }

    #[test]
    fn ask_user_gets_originating_rpc_channel_when_omitted() {
        // Regression: without this, ask_user picks channels.iter().next()
        // (HashMap order) and can hang on an external channel while the user
        // is waiting in the RPC/Code session that issued the turn.
        let mut args = serde_json::json!({
            "question": "Pick one",
            "choices": ["A", "B"],
        });
        maybe_inject_originating_channel("ask_user", &mut args, "rpc");
        assert_eq!(args["channel"], "rpc");
    }

    #[test]
    fn ask_user_does_not_override_explicit_channel() {
        let mut args = serde_json::json!({
            "question": "Pick one",
            "channel": "telegram.default",
        });
        maybe_inject_originating_channel("ask_user", &mut args, "rpc");
        assert_eq!(args["channel"], "telegram.default");
    }

    #[test]
    fn ask_user_skips_generic_agent_channel_name() {
        let mut args = serde_json::json!({ "question": "hi" });
        maybe_inject_originating_channel("ask_user", &mut args, "agent");
        assert!(args.get("channel").is_none());
    }

    #[test]
    fn ask_user_on_channel_turn_gets_reply_target_as_recipient() {
        // Channel turns have a real chat behind them: an empty-recipient
        // question delivers nowhere (there is no CLI/ACP back-channel), which
        // is how "the bot asked me nothing and hung" failures happen.
        let mut args = serde_json::json!({ "question": "Are you coming to dinner?" });
        maybe_inject_channel_delivery_defaults(
            "ask_user",
            &mut args,
            "whatsapp",
            Some("default"),
            Some("207764949356757@lid"),
        );
        assert_eq!(args["recipient"], "207764949356757@lid");
        assert_eq!(args["channel"], "whatsapp");
    }

    #[test]
    fn ask_user_explicit_recipient_is_never_overridden() {
        let mut args = serde_json::json!({
            "question": "Are you coming to dinner?",
            "recipient": "+6593826756@s.whatsapp.net",
        });
        maybe_inject_channel_delivery_defaults(
            "ask_user",
            &mut args,
            "whatsapp",
            Some("default"),
            Some("207764949356757@lid"),
        );
        assert_eq!(args["recipient"], "+6593826756@s.whatsapp.net");
    }

    #[test]
    fn ask_user_without_reply_target_keeps_back_channel_behavior() {
        // CLI / ACP / RPC turns pass no reply target: the tool must keep its
        // blocking back-channel semantics, so no recipient may be injected.
        let mut args = serde_json::json!({ "question": "Proceed?" });
        maybe_inject_channel_delivery_defaults("ask_user", &mut args, "cli", None, None);
        assert!(args.get("recipient").is_none());
    }

    #[test]
    fn non_ask_user_tools_never_get_recipient_injected() {
        for tool in ["poll", "escalate_to_human", "cron_add", "shell"] {
            let mut args = serde_json::json!({ "question": "x" });
            maybe_inject_channel_delivery_defaults(
                tool,
                &mut args,
                "whatsapp",
                Some("default"),
                Some("chat-9"),
            );
            assert!(args.get("recipient").is_none(), "tool={tool}");
        }
    }

    #[test]
    fn originating_channel_tools_are_inventoried() {
        let missing: Vec<&str> = super::ORIGINATING_CHANNEL_TOOLS
            .iter()
            .copied()
            .filter(|name| !zeroclaw_tools::inventory::is_builtin_tool_name(name))
            .collect();
        assert!(
            missing.is_empty(),
            "ORIGINATING_CHANNEL_TOOLS names tools missing from the built-in inventory: {missing:?}"
        );
    }

    #[test]
    fn poll_and_escalate_also_get_originating_channel() {
        for tool in ["poll", "escalate_to_human"] {
            let mut args = serde_json::json!({ "question": "q" });
            maybe_inject_originating_channel(tool, &mut args, "acp");
            assert_eq!(args["channel"], "acp", "tool={tool}");
        }
    }

    #[test]
    fn injected_channel_matches_escalate_tool_schema_parameter() {
        // Guard against the injection being a no-op: `escalate_to_human` must
        // actually declare the `channel` parameter we inject, otherwise the
        // helper "passes" while the tool ignores the key and falls back to
        // channels.iter().next(). Behavioural coverage of the routing itself
        // lives in zeroclaw-tools::escalate tests.
        let tool = zeroclaw_tools::escalate::EscalateToHumanTool::new(
            std::sync::Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
            vec![],
            std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
        );
        let schema = zeroclaw_api::tool::Tool::parameters_schema(&tool);
        assert!(
            schema["properties"]["channel"].is_object(),
            "escalate_to_human must declare the injected `channel` parameter; schema: {schema}"
        );
    }
}
