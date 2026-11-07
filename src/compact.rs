//! Making a long conversation fit the model's window: old tool results cleared without a model call, and the older part summarised by the model.

use crate::context::Item;

/// Separates user messages inside a summary item.
const SEPARATOR: &str = "\n\n---\n\n";

/// Rough size of a conversation in tokens: a quarter of its characters,
/// a flat count per image, and a few for each item's framing.
pub fn tokens(items: &[Item]) -> u64 {
    let mut chars: u64 = 0;
    let mut images: u64 = 0;
    for item in items {
        match item {
            Item::System(t) | Item::User(t) => chars += t.chars().count() as u64,
            Item::Assistant(r) => {
                chars += (r.text.chars().count() + r.reasoning.chars().count()) as u64;
                for c in &r.calls {
                    chars += (c.name.chars().count() + c.args.chars().count()) as u64;
                }
            }
            Item::ToolResult { content, .. } => chars += content.chars().count() as u64,
            Item::Image { .. } => images += 1,
        }
    }
    chars.div_ceil(4) + images * 1_500 + items.len() as u64 * 4
}

/// Start of the text that replaces a cleared tool result.
pub const CLEARED: &str = "[Cleared to save room in the context:";

/// Clears the content of tool results that sit beyond the last `keep`
/// tokens and are long. Returns each cleared index with its original
/// content, in ascending order. Item 0 is never touched.
pub fn clear_old_results(items: &mut [Item], keep: u64) -> Vec<(usize, String)> {
    let mut sum = 0;
    let mut plan = Vec::new();
    for i in (1..items.len()).rev() {
        sum += tokens(&items[i..=i]);
        if sum <= keep {
            continue;
        }
        let Item::ToolResult { call_id, content } = &items[i] else {
            continue;
        };
        if content.chars().count() <= 800 || content.starts_with(CLEARED) {
            continue;
        }
        let lines = content.lines().count();
        let name = tool_name(&items[..i], call_id);
        let text = format!(
            "{CLEARED} {lines} lines of {name} output. Run the tool again if they are needed.]"
        );
        plan.push((i, text));
    }
    let mut cleared = Vec::new();
    for (i, text) in plan {
        if let Item::ToolResult { content, .. } = &mut items[i] {
            let original = std::mem::replace(content, text);
            cleared.push((i, original));
        }
    }
    cleared.reverse();
    cleared
}

/// The name of the tool call with this id, or "a tool" if none is found.
fn tool_name(items: &[Item], call_id: &str) -> String {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Assistant(r) => r
                .calls
                .iter()
                .find(|c| c.id == call_id)
                .map(|c| c.name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "a tool".to_string())
}

/// Index where the kept tail starts, or None when there is nothing
/// before the tail to summarise. The tail starts at a user or assistant
/// item, never at a tool result or image.
pub fn cut_point(items: &[Item], keep: u64) -> Option<usize> {
    let mut sum = 0;
    let mut start = None;
    for i in (0..items.len()).rev() {
        sum += tokens(&items[i..=i]);
        if sum >= keep {
            start = Some(i);
            break;
        }
    }
    let mut i = start?;
    while i > 1 && !matches!(items[i], Item::User(_) | Item::Assistant(_)) {
        i -= 1;
    }
    (i > 1).then_some(i)
}

/// The user messages in order. Messages inside an earlier summary are
/// included in place of the summary.
pub fn user_messages(items: &[Item]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        let Item::User(text) = item else {
            continue;
        };
        if !text.starts_with(SUMMARY_HEAD) {
            out.push(text.clone());
            continue;
        }
        let marker = format!("\n\n{USER_HEAD}\n\n");
        if let Some(pos) = text.find(&marker) {
            out.extend(
                text[pos + marker.len()..]
                    .split(SEPARATOR)
                    .map(str::to_string),
            );
        }
    }
    out
}

/// The first line of the user message that holds a summary.
pub const SUMMARY_HEAD: &str = "[The conversation so far was compacted to fit the context window. The summary below is what came before; the messages after it are as they were.]";
/// Heads the user's own messages, kept word for word.
pub const USER_HEAD: &str = "The user's messages in that part, word for word, oldest first:";

/// One user message holding the summary and the user's own messages.
/// If the messages are longer than `max_chars` in total, the oldest are
/// shortened first.
pub fn summary_item(summary: &str, users: &[String], max_chars: usize) -> Item {
    let mut users = users.to_vec();
    let mut total: usize = users.iter().map(|u| u.chars().count()).sum();
    for u in users.iter_mut() {
        if total <= max_chars {
            break;
        }
        let n = u.chars().count();
        let head: String = u.chars().take(300).collect();
        let short = format!(
            "{head} [\u{2026}{} more characters left out]",
            n.saturating_sub(300)
        );
        if n <= 300 || short.chars().count() >= n {
            continue;
        }
        total = total - n + short.chars().count();
        *u = short;
    }
    let mut text = format!("{SUMMARY_HEAD}\n\n{}", summary.trim());
    if !users.is_empty() {
        text.push_str(&format!("\n\n{USER_HEAD}\n\n{}", users.join(SEPARATOR)));
    }
    Item::User(text)
}

/// Replaces `items[1..cut]` with one summary item and returns the removed
/// items. Does nothing and returns an empty Vec if `cut` is out of range.
pub fn apply(items: &mut Vec<Item>, cut: usize, summary: &str, max_user_chars: usize) -> Vec<Item> {
    if cut > items.len() || cut < 2 {
        return Vec::new();
    }
    let users = user_messages(&items[1..cut]);
    let item = summary_item(summary, &users, max_user_chars);
    items.splice(1..cut, std::iter::once(item)).collect()
}

/// The text a model is sent, last, to write the summary.
pub const PROMPT: &str = "\
Stop here: the conversation is about to be cut to fit your context window. Write the summary that will \
replace everything above this message except the system prompt. The user's own messages are kept word for word \
next to it, so do not repeat them; capture everything else a capable engineer would need to carry on the work \
without seeing the original. Do not call tools, do not answer or continue the conversation, and output only \
the summary, in this structure:

## Goal
What the user wants overall and in this task, including requirements and constraints they stated or confirmed.

## Decisions
What was decided and why, including approaches tried and rejected, so they are not proposed again.

## State of the work
What is done and verified, what is done but unverified, what is in progress, and what is left. Name files, \
functions and commands exactly.

## Files and code
Each file read or changed that still matters: its path, what it holds or what changed. Quote code only when \
the exact text matters later (a signature, an error, a value).

## Errors and fixes
Errors met, their causes and the fixes, and anything that still fails.

## Facts learned
Facts about the environment, tools, APIs or data found along the way that would be costly to find again.

## Next step
The exact next action, as the work stood when this message came.

Be specific and complete; prefer exact names, paths, numbers and quotes over description. Leave out pleasantries \
and anything no longer relevant.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Reply, ToolCall};

    fn user(s: &str) -> Item {
        Item::User(s.to_string())
    }

    fn sys(s: &str) -> Item {
        Item::System(s.to_string())
    }

    fn asst(text: &str) -> Item {
        Item::Assistant(Reply {
            text: text.to_string(),
            ..Reply::default()
        })
    }

    fn call(id: &str, name: &str) -> Item {
        Item::Assistant(Reply {
            calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args: "{}".to_string(),
            }],
            ..Reply::default()
        })
    }

    fn result(id: &str, content: &str) -> Item {
        Item::ToolResult {
            call_id: id.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn tokens_counts_text_and_images() {
        assert_eq!(tokens(&[user("abcd")]), 5);
        let image = Item::Image {
            name: "p.png".into(),
            mime: "image/png".into(),
            data: "AAAA".into(),
        };
        assert_eq!(tokens(&[image]), 1504);
    }

    #[test]
    fn clear_old_results_clears_only_old_long_results() {
        let content: String = (0..10)
            .map(|_| "x".repeat(100))
            .collect::<Vec<_>>()
            .join("\n");
        let mut items = vec![
            sys("s"),
            user("u"),
            call("1", "read"),
            result("1", &content),
            user("x"),
        ];
        let res = clear_old_results(&mut items, 10);
        assert_eq!(res, vec![(3, content.clone())]);
        match &items[3] {
            Item::ToolResult { content, .. } => {
                assert!(content.starts_with(CLEARED));
                assert!(content.contains("10 lines of read output"));
            }
            other => panic!("unexpected item {other:?}"),
        }
        assert!(clear_old_results(&mut items, 10).is_empty());

        let mut short = vec![sys("s"), call("1", "read"), result("1", "short"), user("x")];
        assert!(clear_old_results(&mut short, 0).is_empty());
        assert_eq!(short[2], result("1", "short"));

        let mut kept = vec![
            sys("s"),
            call("1", "read"),
            result("1", &content),
            user("x"),
        ];
        assert!(clear_old_results(&mut kept, u64::MAX).is_empty());
    }

    #[test]
    fn cut_point_finds_user_or_assistant_boundary() {
        let big = "a".repeat(400);
        let items = vec![
            sys("s"),
            user(&big),
            asst("b"),
            result("1", "c"),
            user(&"d".repeat(400)),
        ];
        let keep = tokens(&items[4..]);
        assert_eq!(cut_point(&items, keep), Some(4));

        let items = vec![
            sys("s"),
            user("a"),
            call("1", "read"),
            result("1", &"c".repeat(400)),
            user("small"),
        ];
        assert_eq!(cut_point(&items, 50), Some(2));

        assert_eq!(cut_point(&items, 1_000_000), None);
    }

    #[test]
    fn compacting_twice_keeps_user_messages_in_order() {
        let mut items = vec![sys("sys"), user("first"), asst("ok"), user("second")];
        apply(&mut items, 3, "one", 100_000);
        items.push(user("third"));
        items.push(asst("ok"));
        apply(&mut items, 4, "two", 100_000);
        assert_eq!(items.len(), 3);
        assert_eq!(
            user_messages(&items),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn summary_item_shortens_oldest_first() {
        let a = "a".repeat(1000);
        let b = "b".repeat(1000);
        let text = match summary_item("s", &[a, b.clone()], 1500) {
            Item::User(t) => t,
            other => panic!("unexpected item {other:?}"),
        };
        let short = format!("{} [\u{2026}700 more characters left out]", "a".repeat(300));
        assert!(text.contains(&short));
        assert!(!text.contains(&"a".repeat(301)));
        assert!(text.contains(&b));
    }

    #[test]
    fn apply_returns_removed_items() {
        let mut items = vec![sys("sys"), user("first"), asst("ok"), user("tail")];
        let removed = apply(&mut items, 3, "sum", 100_000);
        assert_eq!(removed, vec![user("first"), asst("ok")]);
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[1], Item::User(t) if t.starts_with(SUMMARY_HEAD)));
        assert_eq!(items[2], user("tail"));

        assert!(apply(&mut items, 1, "x", 10).is_empty());
        assert!(apply(&mut items, 99, "x", 10).is_empty());
        assert_eq!(items.len(), 3);
    }
}
