//! Client-side accumulated conversation transcript and gap pagination.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::chord::context::Context;
use crate::chord::json::JsonObject;
use crate::harness::view::{ConversationView, ConversationWindow};
use crate::ids::{EntryId, id_from_number};
use crate::session::{SessionFuture, SessionResult};
use crate::types::{ConversationRecord, EntryQuery, EntryRecord, Page, ScanOrder};

fn next_id(id: EntryId) -> Option<EntryId> {
    id.get().checked_add(1).map(id_from_number)
}

fn previous_id(id: EntryId) -> Option<EntryId> {
    id.get().checked_sub(1).map(id_from_number)
}

/// One entry, an exclusive gap, or an earlier-page placeholder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TranscriptItem {
    Entry { entry: EntryRecord },
    Gap { after: EntryId, up_to: EntryId },
    Earlier,
}

/// The client's whole transcript instead of one observer's window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptView {
    pub conversation: ConversationRecord,
    pub items: Vec<TranscriptItem>,
    pub docs: BTreeMap<String, JsonObject>,
}

/// Fork-aware entry loader used by earlier and gap pagination.
pub type EntryLoader = Arc<
    dyn for<'a> Fn(EntryQuery, usize, &'a Context) -> SessionFuture<'a, Page<EntryRecord>>
        + Send
        + Sync,
>;

/// Accumulated entries from every view received so far.
pub struct ConversationTranscript {
    load: EntryLoader,
    page_size: usize,
    entries: Vec<EntryRecord>,
    ids: BTreeSet<EntryId>,
    gaps: Vec<(EntryId, EntryId)>,
    started: bool,
    low: Option<EntryId>,
    tail: Option<EntryId>,
    last: Option<ConversationView>,
    loading: Arc<tokio::sync::Mutex<()>>,
}

impl ConversationTranscript {
    pub fn new(load: EntryLoader, page_size: Option<usize>) -> SessionResult<Self> {
        let page_size = page_size.unwrap_or(50);
        if page_size < 1 {
            return Err(crate::session::SessionError::message(
                "Page size must be an integer >= 1",
            ));
        }
        Ok(Self {
            load,
            page_size,
            entries: Vec::new(),
            ids: BTreeSet::new(),
            gaps: Vec::new(),
            started: false,
            low: None,
            tail: None,
            last: None,
            loading: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn newest(&self) -> Option<EntryId> {
        self.tail
    }

    pub fn resume(&self) -> Option<ConversationWindow> {
        if !self.started {
            return None;
        }
        Some(match self.tail {
            Some(newest) => ConversationWindow {
                from: next_id(newest),
                ..Default::default()
            },
            None => ConversationWindow::default(),
        })
    }

    pub fn merge(&mut self, view: ConversationView) -> TranscriptView {
        let high = view.entries.last().map(|entry| entry.id).or(view.before);
        self.cover(view.before, high, &view.entries);
        self.last = Some(view);
        self.result()
    }

    pub fn load_earlier<'a>(
        &'a mut self,
        context: &'a Context,
    ) -> SessionFuture<'a, TranscriptView> {
        Box::pin(async move {
            let loading = Arc::clone(&self.loading);
            let _guard = loading.lock().await;
            if self.last.is_none() {
                return Err(crate::session::SessionError::message(
                    "ConversationTranscript: merge a view first",
                ));
            }
            let Some(low) = self.low else {
                return Ok(self.result());
            };
            let page = (self.load)(
                EntryQuery {
                    conversation_id: self
                        .last
                        .as_ref()
                        .map(|view| view.conversation.id)
                        .unwrap_or(crate::ids::ROOT_CONVERSATION_ID),
                    min_entry_id: None,
                    max_entry_id: Some(low),
                    order: Some(ScanOrder::Descending),
                },
                self.page_size,
                context,
            )
            .await?;
            let mut entries = page.items;
            entries.reverse();
            if page.next.is_none() || entries.is_empty() {
                self.cover(None, Some(low), &entries);
            } else if let Some(oldest) = entries.first() {
                self.cover(previous_id(oldest.id), Some(low), &entries);
            }
            Ok(self.result())
        })
    }

    pub fn fill_gap<'a>(
        &'a mut self,
        after: EntryId,
        up_to: EntryId,
        context: &'a Context,
    ) -> SessionFuture<'a, TranscriptView> {
        Box::pin(async move {
            let loading = Arc::clone(&self.loading);
            let _guard = loading.lock().await;
            if self.last.is_none() {
                return Err(crate::session::SessionError::message(
                    "ConversationTranscript: merge a view first",
                ));
            }
            let Some((current_after, current_up_to)) = self
                .gaps
                .iter()
                .find(|(start, end)| {
                    (*start, *end) == (after, up_to) || (*start < up_to && after < *end)
                })
                .map(|(start, end)| (*start, *end))
            else {
                return Ok(self.result());
            };
            let page = (self.load)(
                EntryQuery {
                    conversation_id: self
                        .last
                        .as_ref()
                        .map(|view| view.conversation.id)
                        .unwrap_or(crate::ids::ROOT_CONVERSATION_ID),
                    min_entry_id: next_id(current_after),
                    max_entry_id: previous_id(current_up_to),
                    order: Some(ScanOrder::Ascending),
                },
                self.page_size,
                context,
            )
            .await?;
            let newest = page.items.last().map(|entry| entry.id);
            if page.next.is_none() || newest.is_none() {
                self.cover(Some(current_after), previous_id(current_up_to), &page.items);
            } else {
                self.cover(Some(current_after), newest, &page.items);
            }
            Ok(self.result())
        })
    }

    fn cover(&mut self, lo: Option<EntryId>, hi: Option<EntryId>, added: &[EntryRecord]) {
        let fresh = added
            .iter()
            .filter(|entry| self.ids.insert(entry.id))
            .cloned()
            .collect::<Vec<_>>();
        let fresh_count = fresh.len();
        let append = fresh.is_empty()
            || self.entries.is_empty()
            || fresh
                .first()
                .is_some_and(|entry| self.entries.last().is_some_and(|last| entry.id > last.id));
        if append {
            self.entries.extend(fresh);
        } else {
            self.entries = merge_by_id(&self.entries, &fresh);
        }

        if !self.started
            || (self.entries.len() == fresh_count && self.gaps.is_empty() && lo >= self.tail)
        {
            self.started = true;
            self.low = lo;
            self.tail = self.tail.max(hi);
        } else if lo > self.tail {
            let up_to = added
                .first()
                .map(|entry| entry.id)
                .or_else(|| lo.and_then(next_id));
            if let (Some(after), Some(up_to)) = (self.tail, up_to) {
                self.gaps.push((after, up_to));
            }
            self.tail = hi;
        } else if hi < self.low {
            let up_to = self
                .entries
                .iter()
                .find(|entry| Some(entry.id) > hi)
                .map(|entry| entry.id)
                .or_else(|| self.low.and_then(next_id));
            if let (Some(after), Some(up_to)) = (hi, up_to) {
                self.gaps.insert(0, (after, up_to));
            }
            self.low = lo;
        } else {
            self.gaps = self.subtract(lo, hi);
            if lo < self.low {
                self.low = lo;
            }
            if hi > self.tail {
                self.tail = hi;
            }
        }
    }

    fn subtract(&self, lo: Option<EntryId>, hi: Option<EntryId>) -> Vec<(EntryId, EntryId)> {
        if hi <= lo {
            return self.gaps.clone();
        }
        let mut gaps = Vec::new();
        for (after, up_to) in &self.gaps {
            if hi <= Some(*after) || lo.and_then(next_id) >= Some(*up_to) {
                gaps.push((*after, *up_to));
                continue;
            }
            if lo > Some(*after) {
                let first_known = self
                    .entries
                    .iter()
                    .find(|entry| Some(entry.id) > lo)
                    .map(|entry| entry.id)
                    .or_else(|| lo.and_then(next_id))
                    .unwrap_or(*up_to);
                gaps.push((*after, first_known.min(*up_to)));
            }
            if hi.and_then(next_id) < Some(*up_to)
                && let Some(after) = hi
            {
                gaps.push((after, *up_to));
            }
        }
        gaps
    }

    fn result(&self) -> TranscriptView {
        let last = self.last.as_ref().expect("merge a view first");
        let mut items = self
            .low
            .map(|_| TranscriptItem::Earlier)
            .into_iter()
            .collect::<Vec<_>>();
        let mut gaps = self.gaps.iter().peekable();
        for entry in &self.entries {
            while gaps.peek().is_some_and(|(after, _)| *after < entry.id) {
                let (after, up_to) = gaps.next().expect("peeked gap");
                items.push(TranscriptItem::Gap {
                    after: *after,
                    up_to: *up_to,
                });
            }
            items.push(TranscriptItem::Entry {
                entry: entry.clone(),
            });
        }
        for (after, up_to) in gaps {
            items.push(TranscriptItem::Gap {
                after: *after,
                up_to: *up_to,
            });
        }
        TranscriptView {
            conversation: last.conversation.clone(),
            items,
            docs: last.docs.clone(),
        }
    }
}

fn merge_by_id(current: &[EntryRecord], added: &[EntryRecord]) -> Vec<EntryRecord> {
    let mut merged = Vec::with_capacity(current.len() + added.len());
    let (mut left, mut right) = (0, 0);
    while left < current.len() || right < added.len() {
        let take_left =
            right >= added.len() || (left < current.len() && current[left].id <= added[right].id);
        if take_left {
            merged.push(current[left].clone());
            left += 1;
        } else {
            merged.push(added[right].clone());
            right += 1;
        }
    }
    merged
}
