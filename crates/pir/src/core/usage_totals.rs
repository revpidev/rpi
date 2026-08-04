//! Usage aggregation for session statistics.
//!
//! Port of `packages/coding-agent/src/core/usage-totals.ts` @ pi 0.82.1
//! (2efa728).

use pir_agent::messages::AgentMessage;
use pir_agent::session::SessionEntry;
use pir_ai::types::Usage;

/// `UsageTotals` (usage-totals.ts:4-10).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

/// `createUsageTotals` (usage-totals.ts:12-20).
pub fn create_usage_totals() -> UsageTotals {
    UsageTotals::default()
}

/// `addUsageToTotals` (usage-totals.ts:22-28).
pub fn add_usage_to_totals(totals: &mut UsageTotals, usage: &Usage) {
    totals.input += usage.input;
    totals.output += usage.output;
    totals.cache_read += usage.cache_read;
    totals.cache_write += usage.cache_write;
    totals.cost += usage.cost.total;
}

/// `UsageCostBreakdownEntry` (usage-totals.ts:30-34).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

/// `getUsageCostBreakdown` (usage-totals.ts:37-70): group attributable
/// assistant usage by model and all other usage into a separate bucket.
pub fn get_usage_cost_breakdown(entries: &[SessionEntry]) -> Vec<UsageCostBreakdownEntry> {
    let mut totals_by_key: Vec<(String, UsageTotals)> = Vec::new();

    for entry in entries {
        let (key, usage): (Option<String>, Option<&Usage>) = match entry {
            SessionEntry::Message(message_entry) => match &message_entry.message {
                AgentMessage::Assistant(assistant) => (
                    Some(format!(
                        "{}/{}",
                        assistant.provider,
                        assistant
                            .response_model
                            .as_ref()
                            .unwrap_or(&assistant.model)
                    )),
                    Some(&assistant.usage),
                ),
                AgentMessage::ToolResult(tool_result) => match &tool_result.usage {
                    Some(usage) => (Some("Tools/summaries".to_owned()), Some(usage)),
                    None => (None, None),
                },
                _ => (None, None),
            },
            SessionEntry::BranchSummary(branch_summary) => match &branch_summary.usage {
                Some(usage) => (Some("Tools/summaries".to_owned()), Some(usage)),
                None => (None, None),
            },
            SessionEntry::Compaction(compaction) => match &compaction.usage {
                Some(usage) => (Some("Tools/summaries".to_owned()), Some(usage)),
                None => (None, None),
            },
            _ => (None, None),
        };
        let (Some(key), Some(usage)) = (key, usage) else {
            continue;
        };

        if let Some((_, totals)) = totals_by_key.iter_mut().find(|(k, _)| *k == key) {
            add_usage_to_totals(totals, usage);
        } else {
            let mut totals = create_usage_totals();
            add_usage_to_totals(&mut totals, usage);
            totals_by_key.push((key, totals));
        }
    }

    let mut breakdown: Vec<UsageCostBreakdownEntry> = totals_by_key
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            key,
            cost: totals.cost,
            tokens: totals.input + totals.output + totals.cache_read + totals.cache_write,
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    breakdown.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    breakdown
}
