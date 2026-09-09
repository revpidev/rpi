//! Shared test helpers for the TE28 prerequisite-verification binaries.
//!
//! `RecordingBridge` implements the full `UiBridge` surface: dialogs pop
//! scripted answers (recording order + max concurrency), everything else
//! no-ops. It is deliberately NOT `is_noop()` so the host reports `hasUI`.
//!
//! Each test binary compiles its own copy; helpers unused by one binary are
//! expected (the dialogs binary uses the recording API, the l0 binary only
//! needs a non-noop bridge).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi_agent::types::AgentToolResult;
use rpi_ai::types::ToolResultContent;
use rpi_ext_host::api::{
    ExtensionWidgetOptions, NotifyType, SetThemeResult, TerminalInputHandler, ThemeInfo, UiBridge,
    UiDialogOptions, Unsubscribe, WidgetContent, WorkingIndicatorOptions,
};
use serde_json::Value;

/// Records dialog order + concurrency; pops scripted answers per `select`.
pub struct RecordingBridge {
    selects: Mutex<Vec<String>>,
    answers: Mutex<VecDeque<Option<String>>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl RecordingBridge {
    /// Build with one scripted answer per expected `select` (`None` = cancel).
    pub fn new(answers: Vec<Option<&str>>) -> Arc<Self> {
        Arc::new(Self {
            selects: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.into_iter().map(|a| a.map(str::to_owned)).collect()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        })
    }

    /// Titles of the dialogs opened so far, in order.
    pub fn selects(&self) -> Vec<String> {
        self.selects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Highest number of `select` calls in flight at once.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl UiBridge for RecordingBridge {
    async fn select(
        &self,
        title: &str,
        _options: &[String],
        _opts: Option<UiDialogOptions>,
    ) -> Option<String> {
        let concurrent = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(concurrent, Ordering::SeqCst);
        self.selects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(title.to_owned());
        // Yield so a hypothetical concurrent caller would interleave.
        tokio::task::yield_now().await;
        let answer = self
            .answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .flatten();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        answer
    }

    async fn confirm(&self, _t: &str, _m: &str, _o: Option<UiDialogOptions>) -> bool {
        false
    }
    async fn input(
        &self,
        _t: &str,
        _p: Option<&str>,
        _o: Option<UiDialogOptions>,
    ) -> Option<String> {
        None
    }
    fn notify(&self, _m: &str, _k: NotifyType) {}
    fn on_terminal_input(&self, _h: TerminalInputHandler) -> Unsubscribe {
        Box::new(|| {})
    }
    fn set_status(&self, _k: &str, _t: Option<&str>) {}
    fn set_working_message(&self, _m: Option<&str>) {}
    fn set_working_visible(&self, _v: bool) {}
    fn set_working_indicator(&self, _o: Option<WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
    fn set_widget(&self, _k: &str, _c: Option<WidgetContent>, _o: Option<ExtensionWidgetOptions>) {}
    fn set_footer(&self, _c: Option<Value>) {}
    fn set_header(&self, _c: Option<Value>) {}
    fn set_title(&self, _t: &str) {}
    async fn custom(&self, _c: Value, _o: Option<Value>) -> Option<Value> {
        None
    }
    fn paste_to_editor(&self, _t: &str) {}
    fn set_editor_text(&self, _t: &str) {}
    fn get_editor_text(&self) -> String {
        String::new()
    }
    async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
        None
    }
    fn add_autocomplete_provider(&self, _p: Value) {}
    fn set_editor_component(&self, _c: Option<Value>) {}
    fn get_editor_component(&self) -> Option<Value> {
        None
    }
    fn theme(&self) -> Value {
        Value::Null
    }
    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        Vec::new()
    }
    fn get_theme(&self, _n: &str) -> Option<Value> {
        None
    }
    fn set_theme(&self, _t: Value) -> SetThemeResult {
        SetThemeResult {
            success: false,
            error: None,
        }
    }
    fn get_tools_expanded(&self) -> bool {
        false
    }
    fn set_tools_expanded(&self, _e: bool) {}
}

/// Concatenated text blocks of a tool result.
pub fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => text.text.clone(),
            _ => String::new(),
        })
        .collect()
}
