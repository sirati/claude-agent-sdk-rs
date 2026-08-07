//! Smoke test: confirms the one-shot `query()` API survives the CLI's
//! `rate_limit_event` message instead of crashing (the bug fixed by
//! porting Python SDK commit 146e3d6). Not part of the numbered example
//! series' usual feature tour — kept minimal on purpose.

use claude_agent_sdk::{ClaudeAgentOptions, Message, query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = ClaudeAgentOptions::builder()
        .model("claude-haiku-4-5-20251001")
        .max_turns(1)
        .build();

    let messages = query("Reply with exactly one word: hello", Some(options)).await?;

    println!("Received {} messages", messages.len());
    for message in &messages {
        match message {
            Message::RateLimitEvent(event) => {
                println!(
                    "  RateLimitEvent: status={}",
                    event.rate_limit_info.status
                );
            }
            Message::Result(result) => {
                println!(
                    "  Result: is_error={} session_id={}",
                    result.is_error, result.session_id
                );
            }
            Message::Unknown => {
                println!("  Unknown (forward-compatible skip target)");
            }
            other => {
                println!("  {:?}", std::mem::discriminant(other));
            }
        }
    }

    println!("query() completed successfully - no crash.");
    Ok(())
}
