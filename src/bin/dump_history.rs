//! Standalone transcript dumper.
//!
//! Opens the Iron Clad Fjall history keyspace at
//! `~/.ironclad/history-index/` and prints the most recent
//! conversations as plain markdown. Doesn't need the gateway up —
//! just reads the on-disk store directly.
//!
//! Usage:
//!   cargo run --release --bin dump_history
//!   cargo run --release --bin dump_history -- --last 3
//!   cargo run --release --bin dump_history -- --conv <uuid>

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use fjall::{Config, PartitionCreateOptions};
use serde::Deserialize;
use uuid::Uuid;

/// Mirrors the private `ConvRecord` in `src/history/fjall_store.rs`.
/// Serde uses field names for JSON, so this round-trips correctly
/// even though it's a separate struct.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ConvRecord {
    id: Uuid,
    channel: String,
    user_id: String,
    thread_id: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MsgRecord {
    id: Uuid,
    conversation_id: Uuid,
    role: String,
    content: String,
    created_at: DateTime<Utc>,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut last_n: usize = 1;
    let mut filter_conv: Option<Uuid> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--last" => {
                last_n = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                i += 2;
            }
            "--conv" => {
                filter_conv = args.get(i + 1).and_then(|s| Uuid::parse_str(s).ok());
                i += 2;
            }
            _ => i += 1,
        }
    }

    let home = dirs::home_dir().expect("could not resolve $HOME");
    let path = home.join(".ironclad").join("history-index");
    eprintln!("opening Fjall keyspace at {}", path.display());

    let keyspace = Config::new(&path).open()?;
    let conversations =
        keyspace.open_partition("conversations", PartitionCreateOptions::default())?;
    let messages = keyspace.open_partition("messages", PartitionCreateOptions::default())?;

    // Pull every conversation, sort by last_activity descending.
    let mut convs: Vec<ConvRecord> = Vec::new();
    for kv in conversations.iter() {
        let (_, v) = kv?;
        match serde_json::from_slice::<ConvRecord>(&v) {
            Ok(c) => convs.push(c),
            Err(e) => eprintln!("skipping bad conv record: {e}"),
        }
    }
    convs.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    eprintln!("found {} conversations total", convs.len());

    let targets: Vec<ConvRecord> = if let Some(uuid) = filter_conv {
        convs.into_iter().filter(|c| c.id == uuid).collect()
    } else {
        convs.into_iter().take(last_n).collect()
    };

    for conv in &targets {
        // Prefix-scan messages by conversation id.
        let prefix = conv.id.as_bytes().to_vec();
        let mut msgs: BTreeMap<DateTime<Utc>, MsgRecord> = BTreeMap::new();
        for kv in messages.prefix(&prefix) {
            let (_, v) = kv?;
            match serde_json::from_slice::<MsgRecord>(&v) {
                Ok(m) => {
                    msgs.insert(m.created_at, m);
                }
                Err(e) => eprintln!("skipping bad msg record: {e}"),
            }
        }
        let user_msgs = msgs.values().filter(|m| m.role == "user").count();
        let asst_msgs = msgs
            .values()
            .filter(|m| m.role == "assistant" || m.role == "agent")
            .count();

        println!("# Conversation {}", conv.id);
        println!();
        println!("- **channel**: {}", conv.channel);
        println!("- **user_id**: {}", conv.user_id);
        if let Some(ref t) = conv.thread_id {
            println!("- **thread_id**: {}", t);
        }
        println!(
            "- **created_at**: {}",
            conv.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!(
            "- **last_activity**: {}",
            conv.last_activity.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!(
            "- **turns**: {} user / {} assistant / {} total messages",
            user_msgs,
            asst_msgs,
            msgs.len()
        );
        println!();
        println!("---");
        println!();

        for (ts, m) in msgs.iter() {
            let role_tag = match m.role.as_str() {
                "user" => "USER",
                "assistant" | "agent" => "JARVIS",
                "tool" => "TOOL",
                "system" => "SYSTEM",
                other => other,
            };
            println!("### [{}] {}", ts.format("%H:%M:%S"), role_tag);
            println!();
            // Indent multi-line content as a blockquote so role
            // boundaries stay visually obvious.
            for line in m.content.lines() {
                println!("> {line}");
            }
            println!();
        }
        println!();
    }

    Ok(())
}
