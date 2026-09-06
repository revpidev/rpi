//! Port of `packages/tui/src/components/mouse-region.ts` @ pi 0.85.0+
//! (9841914, introduced by 71026970a).
//!
//! Intentional differences:
//! - The wrapped child is a [`SharedComponent`] (the port's ownership model
//!   for component references; upstream holds a live JS object reference) —
//!   see the V14-14 header note in `tui.rs`.
//! - The handler is a boxed closure (`MouseRegionHandler`) instead of an
//!   arbitrary JS function.
//! - `handleMouse` forwards the child's dispatch result flattened to the
//!   plain [`TuiMouseEventResult`] shape: the dispatch target/focus target
//!   re-attach at the shared root that received the dispatch, so the child's
//!   own target metadata is not needed (upstream passes the child's
//!   `TuiMouseDispatchResult` through verbatim; the observable routing is
//!   identical).

use crate::tui::{
    dispatch_mouse_event, lock_component, Component, SharedComponent, TuiMouseEvent,
    TuiMouseEventResult, TuiMouseHandlerResult,
};

/// `MouseRegionHandler` (mouse-region.ts:10).
pub type MouseRegionHandler = Box<dyn Fn(&TuiMouseEvent) -> Option<TuiMouseEventResult> + Send>;

/// Adds mouse handling to an existing component without changing its
/// rendering (upstream `MouseRegion`, mouse-region.ts:13-31).
pub struct MouseRegion {
    child: SharedComponent,
    on_mouse: MouseRegionHandler,
}

impl MouseRegion {
    /// Upstream `constructor(child, onMouse)` (mouse-region.ts:19-23).
    pub fn new(child: SharedComponent, on_mouse: MouseRegionHandler) -> Self {
        Self { child, on_mouse }
    }

    /// The wrapped child (port addition: callers that keep mutating the
    /// child after wrapping reach it through this handle).
    pub fn child(&self) -> &SharedComponent {
        &self.child
    }
}

impl Component for MouseRegion {
    fn render(&self, width: usize) -> Vec<String> {
        lock_component(&self.child).render(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
        // `const childResult = dispatchMouseEvent(this.child, event);
        //  return childResult ?? this.onMouse(event);` (mouse-region.ts:26-27).
        // The child's dispatch result passes through unchanged (upstream
        // `"target" in result`): its exact target stays the gesture target.
        if let Some(child_result) = dispatch_mouse_event(&self.child, event) {
            return Some(TuiMouseHandlerResult::Forwarded(child_result));
        }
        (self.on_mouse)(event).map(TuiMouseHandlerResult::Event)
    }

    fn invalidate(&mut self) {
        lock_component(&self.child).invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;
    use crate::tui::{
        shared_component, TuiMouseButton, TuiMouseEventResult as Result_, TuiMouseEventType,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn event(event_type: TuiMouseEventType, button: TuiMouseButton) -> TuiMouseEvent {
        TuiMouseEvent {
            event_type,
            button,
            x: 0,
            y: 0,
            screen_x: 0,
            screen_y: 0,
            width: 10,
            height: 1,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: None,
        }
    }

    #[test]
    fn render_passes_through_and_invalidate_forwards() {
        let region = MouseRegion::new(
            shared_component(Text::new("hi", 0, 0, None)),
            Box::new(|_| None),
        );
        // Render passes through: Text pads its single line to the width.
        assert_eq!(region.render(10), vec![format!("hi{}", " ".repeat(8))]);
        // Invalidate must not panic and reaches the child (Text has no
        // cached state; observable via a spy component below).
    }

    #[test]
    fn child_result_wins_over_callback() {
        // The child claims the press; the callback must not run. The
        // child's dispatch result passes through as `Forwarded` — the child
        // stays the gesture target (upstream returns the child's
        // `TuiMouseDispatchResult` verbatim, mouse-region.ts:26).
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let child = shared_component(SpyHandler {
            label: "child",
            result: Some(Result_ {
                handled: true,
                ..Default::default()
            }),
        });
        let mut region = MouseRegion::new(
            child,
            Box::new(move |_| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                None
            }),
        );
        let forwarded =
            match region.handle_mouse(&event(TuiMouseEventType::Press, TuiMouseButton::Left)) {
                Some(crate::tui::TuiMouseHandlerResult::Forwarded(result)) => result,
                _ => panic!("child dispatch results forward verbatim"),
            };
        assert_eq!(
            forwarded.event_result(),
            Result_ {
                handled: true,
                ..Default::default()
            }
        );
        // The child is the dispatch target.
        assert!(Arc::ptr_eq(&forwarded.target.component, region.child()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callback_runs_when_child_declines() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let mut region = MouseRegion::new(
            shared_component(Text::new("hi", 0, 0, None)),
            Box::new(move |event| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                assert_eq!(event.event_type, TuiMouseEventType::Click);
                Some(Result_ {
                    handled: true,
                    ..Default::default()
                })
            }),
        );
        let result =
            match region.handle_mouse(&event(TuiMouseEventType::Click, TuiMouseButton::Left)) {
                Some(crate::tui::TuiMouseHandlerResult::Event(result)) => Some(result),
                Some(crate::tui::TuiMouseHandlerResult::Forwarded(_)) => {
                    panic!("expected Event result")
                }
                None => None,
            };
        assert_eq!(
            result,
            Some(Result_ {
                handled: true,
                ..Default::default()
            })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Minimal handler component for dispatch tests.
    struct SpyHandler {
        label: &'static str,
        result: Option<Result_>,
    }

    impl Component for SpyHandler {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![self.label.to_string()]
        }

        fn handle_mouse(
            &mut self,
            _event: &TuiMouseEvent,
        ) -> Option<crate::tui::TuiMouseHandlerResult> {
            self.result.map(crate::tui::TuiMouseHandlerResult::Event)
        }
    }
}
