//! Contract tests for the layout engine (`rpi_tui::layout`, T30).
//!
//! Intent ports of `external/pi/packages/tui/test/layout.test.ts` @ 4181f66
//! (all 14 cases, coding-standards §12.2 — intent, not line-by-line).
//!
//! Test-level adaptations (not algorithm deviations):
//! - Upstream mocks components as inline `{ render, invalidate }` objects;
//!   here they are local mock components (`CountingLines`, `BigLines`,
//!   `SharedLines`). `SharedLines` replaces upstream's `Text.setText`
//!   mutation, which needs `&mut Text` and is unreachable through a
//!   `SharedComponent`.
//! - "paints only clipped rows from very large scroll content" uses
//!   1_000_000 lines instead of 1_000_000_000: upstream exploits a sparse JS
//!   array (`lines.length = 1e9`, four defined entries); a dense Rust
//!   `Vec<String>` of 1e9 elements would need ~24 GB. 1e6 still makes any
//!   O(content) painting immediately visible.
//! - The transient-scrollbar 1s wait uses `scrollbar_hide_delay: 10ms` +
//!   `sleep(30ms)` + `tick(Instant::now())` — the explicit-deadline timer
//!   model (D-082) instead of a real `setTimeout`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_test_support::vt::strip_ansi;

use rpi_tui::components::h_stack::HStack;
use rpi_tui::components::scroll_view::{
    Follow, ScrollView, ScrollViewOptions, ScrollbarMode, ScrollbarStyleFn,
};
use rpi_tui::components::stack::{StackChild, StackEntryOptions, StackOptions};
use rpi_tui::components::text::Text;
use rpi_tui::components::v_stack::VStack;
use rpi_tui::layout::render_layout_frame;
use rpi_tui::layout_node::{Basis, StackAlign};
use rpi_tui::terminal_image::{
    encode_kitty, register_kitty_image_metadata, KittyEncodeOptions, KittyImageMetadata,
};
use rpi_tui::tui::{shared_component, Component, RenderHandle, SharedComponent};

/// `visibleLines` (layout.test.ts:11-13): strip terminal sequences, trim end.
fn visible_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| strip_ansi(line).trim_end().to_string())
        .collect()
}

fn strip_all(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_ansi(line)).collect()
}

fn noop_render_handle() -> RenderHandle {
    RenderHandle::new(|| {})
}

/// Call a method on the `ScrollView` inside a `SharedComponent`.
fn with_scroll_view<R>(shared: &SharedComponent, f: impl FnOnce(&ScrollView) -> R) -> R {
    let guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scroll_view = guard.as_scroll_view().expect("expected a ScrollView");
    f(scroll_view)
}

fn entry(component: SharedComponent, options: StackEntryOptions) -> StackChild {
    StackChild::Entry(component, options)
}

fn plain(component: SharedComponent) -> StackChild {
    StackChild::Component(component)
}

fn text(content: &str) -> SharedComponent {
    shared_component(Text::new(content, 0, 0, None))
}

// ---- mock components (upstream inline `{ render, invalidate }` objects) ----

/// Fixed lines with a render counter (layout.test.ts:35-41).
struct CountingLines {
    renders: Arc<AtomicU64>,
    lines: Vec<String>,
}

impl Component for CountingLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.lines.clone()
    }
}

fn counting_lines(lines: Vec<String>) -> (SharedComponent, Arc<AtomicU64>) {
    let renders = Arc::new(AtomicU64::new(0));
    (
        shared_component(CountingLines {
            renders: Arc::clone(&renders),
            lines,
        }),
        renders,
    )
}

/// Very large content (layout.test.ts:52-58); see the header note on why the
/// port uses 1e6 lines instead of 1e9.
struct BigLines {
    lines: Vec<String>,
}

impl Component for BigLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// Line list mutable through an exterior handle (replaces `Text.setText` on a
/// shared component; layout.test.ts:226-234, 296-304).
struct SharedLines {
    lines: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct LinesHandle(Arc<Mutex<Vec<String>>>);

impl LinesHandle {
    fn set_lines(&self, lines: Vec<String>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = lines;
    }
}

impl Component for SharedLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn shared_lines(lines: Vec<String>) -> (SharedComponent, LinesHandle) {
    let lines = Arc::new(Mutex::new(lines));
    (
        shared_component(SharedLines {
            lines: Arc::clone(&lines),
        }),
        LinesHandle(lines),
    )
}

// ---- the 14 upstream cases ----

/// layout.test.ts:16-32.
#[test]
fn allocates_vertical_grow_space_deterministically() {
    let frame = render_layout_frame(
        &shared_component(VStack::new(
            vec![
                entry(
                    text("top"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(1.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
                entry(
                    text("body"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        )),
        10,
        4,
        noop_render_handle(),
    );

    let heights: Vec<usize> = frame.root.children.iter().map(|c| c.rect.height).collect();
    assert_eq!(heights, [1, 3]);
    assert_eq!(visible_lines(&frame.lines), ["top", "body", "", ""]);
}

/// layout.test.ts:34-49.
#[test]
fn does_not_render_fixed_basis_scroll_content_during_stack_measurement() {
    let (content, renders) = counting_lines(vec!["one".into(), "two".into(), "three".into()]);
    let transcript = shared_component(ScrollView::new(content, ScrollViewOptions::default()));
    let root = shared_component(VStack::new(
        vec![
            entry(
                transcript,
                StackEntryOptions {
                    basis: Some(Basis::Fixed(0.0)),
                    grow: Some(1.0),
                    ..StackEntryOptions::default()
                },
            ),
            entry(
                text("dock"),
                StackEntryOptions {
                    basis: Some(Basis::Auto),
                    ..StackEntryOptions::default()
                },
            ),
        ],
        StackOptions::default(),
    ));
    render_layout_frame(&root, 10, 3, noop_render_handle());
    assert_eq!(renders.load(Ordering::Relaxed), 1);
}

/// layout.test.ts:51-69.
#[test]
fn paints_only_clipped_rows_from_very_large_scroll_content() {
    let line_count = 1_000_000usize;
    let mut lines = vec![String::new(); line_count];
    lines[line_count - 4] = "before".into();
    lines[line_count - 3] = "visible 1".into();
    lines[line_count - 2] = "visible 2".into();
    lines[line_count - 1] = "visible 3".into();
    let transcript = shared_component(ScrollView::new(
        shared_component(BigLines { lines }),
        ScrollViewOptions {
            follow: Follow::End,
            ..ScrollViewOptions::default()
        },
    ));

    let frame = render_layout_frame(&transcript, 10, 3, noop_render_handle());
    assert_eq!(
        visible_lines(&frame.lines),
        ["visible 1", "visible 2", "visible 3"]
    );
}

/// layout.test.ts:71-87.
#[test]
fn shrinks_entries_to_their_minimum_sizes() {
    let frame = render_layout_frame(
        &shared_component(VStack::new(
            vec![
                entry(
                    text("a1\na2\na3"),
                    StackEntryOptions {
                        shrink: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                entry(
                    text("b1\nb2\nb3"),
                    StackEntryOptions {
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        )),
        10,
        4,
        noop_render_handle(),
    );

    let heights: Vec<usize> = frame.root.children.iter().map(|c| c.rect.height).collect();
    assert_eq!(heights, [1, 3]);
    assert_eq!(visible_lines(&frame.lines), ["a1", "b1", "b2", "b3"]);
}

/// layout.test.ts:89-117.
#[test]
fn includes_nested_minimum_sizes_in_intrinsic_stack_measurement() {
    let dock = shared_component(VStack::new(
        vec![
            plain(text("top1\ntop2\ntop3")),
            entry(
                text("selector"),
                StackEntryOptions {
                    min_size: Some(3.0),
                    ..StackEntryOptions::default()
                },
            ),
            plain(text("below")),
            entry(
                text("footer"),
                StackEntryOptions {
                    min_size: Some(1.0),
                    ..StackEntryOptions::default()
                },
            ),
        ],
        StackOptions::default(),
    ));
    let frame = render_layout_frame(
        &shared_component(VStack::new(
            vec![
                entry(
                    text("body"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                entry(
                    dock,
                    StackEntryOptions {
                        basis: Some(Basis::Auto),
                        min_size: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        )),
        10,
        9,
        noop_render_handle(),
    );

    assert_eq!(
        visible_lines(&frame.lines),
        ["body", "top1", "top2", "top3", "selector", "", "", "below", "footer"]
    );
}

/// layout.test.ts:119-128.
#[test]
fn omits_gaps_around_invisible_entries() {
    let stack = VStack::new(
        vec![
            plain(text("one")),
            entry(
                text("hidden"),
                StackEntryOptions {
                    visible: Some(Arc::new(|_| false)),
                    ..StackEntryOptions::default()
                },
            ),
            plain(text("two")),
        ],
        StackOptions {
            gap: Some(1.0),
            ..StackOptions::default()
        },
    );
    let lines: Vec<String> = stack
        .render(10)
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect();
    assert_eq!(lines, ["one", "", "two"]);
}

/// layout.test.ts:130-146.
#[test]
fn crops_kitty_images_at_a_scroll_views_lower_boundary() {
    let image_id = 124u32;
    let image_line = encode_kitty(
        "AAAA",
        &KittyEncodeOptions {
            columns: Some(2),
            rows: Some(3),
            image_id: Some(image_id),
            move_cursor: Some(false),
        },
    );
    register_kitty_image_metadata(KittyImageMetadata {
        image_id,
        columns: 2,
        rows: 3,
        width_px: 100.0,
        height_px: 100.0,
    });
    let transcript = shared_component(ScrollView::new(
        shared_component(SharedLines {
            lines: Arc::new(Mutex::new(vec![
                "one".into(),
                "two".into(),
                image_line,
                String::new(),
                String::new(),
            ])),
        }),
        ScrollViewOptions::default(),
    ));
    let frame = render_layout_frame(
        &shared_component(VStack::new(
            vec![
                entry(
                    transcript,
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
                plain(text("dock")),
            ],
            StackOptions::default(),
        )),
        20,
        4,
        noop_render_handle(),
    );

    assert!(
        frame.lines[2].contains("y=0,h=34,r=1"),
        "expected cropped kitty controls in line 2, got {:?}",
        frame.lines[2]
    );
}

/// layout.test.ts:148-159.
#[test]
fn composes_horizontal_children_at_allocated_widths() {
    let frame = render_layout_frame(
        &shared_component(HStack::new(
            vec![
                entry(
                    text("left"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(6.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
                entry(
                    text("right"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(6.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        )),
        12,
        1,
        noop_render_handle(),
    );
    assert_eq!(visible_lines(&frame.lines), ["left  right"]);
}

/// layout.test.ts:161-172.
#[test]
fn does_not_paint_zero_width_horizontal_children() {
    let frame = render_layout_frame(
        &shared_component(HStack::new(
            vec![
                entry(
                    text("hidden"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        shrink: Some(0.0),
                        ..StackEntryOptions::default()
                    },
                ),
                entry(
                    text("shown"),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(0.0)),
                        grow: Some(1.0),
                        ..StackEntryOptions::default()
                    },
                ),
            ],
            StackOptions::default(),
        )),
        5,
        1,
        noop_render_handle(),
    );
    assert_eq!(visible_lines(&frame.lines), ["shown"]);
}

/// layout.test.ts:174-191.
#[test]
fn tracks_follow_end_state_and_returns_unused_scroll_delta() {
    let scroll_view = shared_component(ScrollView::new(
        text("1\n2\n3\n4\n5\n6"),
        ScrollViewOptions {
            follow: Follow::End,
            primary: true,
            ..ScrollViewOptions::default()
        },
    ));
    render_layout_frame(&scroll_view, 10, 3, noop_render_handle());
    with_scroll_view(&scroll_view, |sv| {
        assert_eq!(sv.scroll_top(), 3);
        assert!(sv.is_following_end());

        assert_eq!(sv.scroll_by(-2), 0);
        assert_eq!(sv.scroll_top(), 1);
        assert!(!sv.is_following_end());
        assert_eq!(sv.scroll_by(-3), -2);
        assert_eq!(sv.scroll_top(), 0);
        assert_eq!(sv.scroll_by(10), 7);
        assert_eq!(sv.scroll_top(), 3);
        assert!(sv.is_following_end());
    });
}

/// layout.test.ts:193-271 (segments a-i inline).
#[test]
fn renders_a_transient_proportional_scrollbar_without_replacing_cell_content() {
    let source_lines = [
        "abcd界", "abcde2", "abcde3", "abcde4", "abcde5", "abcde6", "abcde7", "abcde8",
    ];
    let content_background = "\x1b[42m";
    let scrollbar_background = "\x1b[48;5;1m";
    let scrollbar_style: ScrollbarStyleFn =
        Arc::new(move |text| format!("{scrollbar_background}{text}\x1b[49m"));
    let content = Text::new(
        source_lines.join("\n"),
        0,
        0,
        Some(Box::new(move |text: &str| {
            format!("{content_background}{text}\x1b[49m")
        })),
    );
    let scroll_view = shared_component(ScrollView::new(
        shared_component(content),
        ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            scrollbar_style: Some(scrollbar_style.clone()),
            scrollbar_hide_delay: Duration::from_millis(10),
            ..ScrollViewOptions::default()
        },
    ));
    let render = || render_layout_frame(&scroll_view, 6, 4, noop_render_handle()).lines;
    let thumb_rows = |lines: &[String]| -> Vec<bool> {
        lines
            .iter()
            .map(|line| line.contains(scrollbar_background))
            .collect()
    };

    // (a) No scroll activity yet: no scrollbar, content passes through.
    let lines = render();
    assert_eq!(thumb_rows(&lines), [false, false, false, false]);
    assert_eq!(strip_all(&lines), source_lines[..4]);

    // (b) Activity shows the thumb; the styled cell keeps its text, and the
    // content background stays outside the scrollbar style.
    with_scroll_view(&scroll_view, |sv| {
        sv.scroll_by(2);
    });
    let lines = render();
    assert_eq!(thumb_rows(&lines), [false, true, true, false]);
    assert_eq!(strip_all(&lines), source_lines[2..6]);
    let (Some(last_content), Some(last_scrollbar)) = (
        lines[1].rfind(content_background),
        lines[1].rfind(scrollbar_background),
    ) else {
        panic!("expected both backgrounds in line 1: {:?}", lines[1]);
    };
    assert!(last_content < last_scrollbar);

    // (c) The transient scrollbar hides once the deadline passes (tick).
    std::thread::sleep(Duration::from_millis(30));
    with_scroll_view(&scroll_view, |sv| sv.tick(Instant::now()));
    let lines = render();
    assert_eq!(thumb_rows(&lines), [false, false, false, false]);

    // (d) At the end the thumb hugs the bottom rows.
    with_scroll_view(&scroll_view, |sv| sv.scroll_to_end());
    let lines = render();
    assert_eq!(thumb_rows(&lines), [false, false, true, true]);
    assert_eq!(strip_all(&lines), source_lines[4..]);

    // (e) follow=end stays pinned across content growth; no scrollbar without
    // scroll activity.
    let (followed_content, followed_handle) =
        shared_lines(source_lines.iter().map(|s| s.to_string()).collect());
    let followed = shared_component(ScrollView::new(
        followed_content,
        ScrollViewOptions {
            follow: Follow::End,
            scrollbar: ScrollbarMode::Auto,
            scrollbar_style: Some(scrollbar_style.clone()),
            ..ScrollViewOptions::default()
        },
    ));
    render_layout_frame(&followed, 6, 4, noop_render_handle());
    with_scroll_view(&followed, |sv| assert_eq!(sv.scroll_top(), 4));
    let mut grown: Vec<String> = source_lines.iter().map(|s| s.to_string()).collect();
    grown.push("abcde9".into());
    followed_handle.set_lines(grown);
    let growth_frame = render_layout_frame(&followed, 6, 4, noop_render_handle());
    with_scroll_view(&followed, |sv| assert_eq!(sv.scroll_top(), 5));
    assert!(growth_frame
        .lines
        .iter()
        .all(|line| !line.contains(scrollbar_background)));

    // (f) Content fitting the viewport never shows the auto scrollbar.
    let fitting = shared_component(ScrollView::new(
        text("1\n2"),
        ScrollViewOptions {
            scrollbar: ScrollbarMode::Auto,
            scrollbar_style: Some(scrollbar_style.clone()),
            ..ScrollViewOptions::default()
        },
    ));
    render_layout_frame(&fitting, 6, 4, noop_render_handle());
    with_scroll_view(&fitting, |sv| {
        sv.scroll_by(1);
    });
    assert!(render_layout_frame(&fitting, 6, 4, noop_render_handle())
        .lines
        .iter()
        .all(|line| !line.contains(scrollbar_background)));

    // (g) scrollbar=always reserves a column and styles the full track when
    // the content fits.
    let always_fitting = shared_component(ScrollView::new(
        text("1\n2"),
        ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            scrollbar_style: Some(scrollbar_style.clone()),
            ..ScrollViewOptions::default()
        },
    ));
    let always_fitting_frame = render_layout_frame(&always_fitting, 6, 4, noop_render_handle());
    assert_eq!(always_fitting_frame.root.children[0].rect.width, 5);
    assert!(always_fitting_frame
        .lines
        .iter()
        .all(|line| line.contains(scrollbar_background)));

    // (h) With overflow the thumb covers track^2/content rows (2 of 4 here).
    let always_overflowing = shared_component(ScrollView::new(
        shared_component(Text::new(source_lines.join("\n"), 0, 0, None)),
        ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            scrollbar_style: Some(scrollbar_style.clone()),
            ..ScrollViewOptions::default()
        },
    ));
    let always_overflowing_frame =
        render_layout_frame(&always_overflowing, 6, 4, noop_render_handle());
    assert_eq!(always_overflowing_frame.root.children[0].rect.width, 5);
    assert_eq!(
        always_overflowing_frame
            .lines
            .iter()
            .filter(|line| line.contains(scrollbar_background))
            .count(),
        2
    );

    // (i) Thumb-height matrix at viewport 20.
    let thumb_height_for = |content_height: usize| -> usize {
        let sized = shared_component(ScrollView::new(
            text(&vec!["x"; content_height].join("\n")),
            ScrollViewOptions {
                scrollbar: ScrollbarMode::Auto,
                scrollbar_style: Some(scrollbar_style.clone()),
                ..ScrollViewOptions::default()
            },
        ));
        render_layout_frame(&sized, 6, 20, noop_render_handle());
        with_scroll_view(&sized, |sv| {
            sv.scroll_by(1);
        });
        render_layout_frame(&sized, 6, 20, noop_render_handle())
            .lines
            .iter()
            .filter(|line| line.contains(scrollbar_background))
            .count()
    };
    assert_eq!(thumb_height_for(21), 19);
    assert_eq!(thumb_height_for(40), 10);
    assert_eq!(thumb_height_for(100), 4);
    assert_eq!(thumb_height_for(400), 2);
}

/// layout.test.ts:273-284.
#[test]
fn updates_reserved_scrollbar_layout_at_runtime() {
    let scroll_view = shared_component(ScrollView::new(
        text("123456"),
        ScrollViewOptions {
            scrollbar: ScrollbarMode::Always,
            ..ScrollViewOptions::default()
        },
    ));
    let render = || {
        render_layout_frame(
            &shared_component(HStack::new(
                vec![plain(scroll_view.clone())],
                StackOptions {
                    align: Some(StackAlign::Start),
                    ..StackOptions::default()
                },
            )),
            6,
            2,
            noop_render_handle(),
        )
    };
    let always = render();
    assert_eq!(visible_lines(&always.lines), ["12345", "6"]);
    assert_eq!(always.root.children[0].rect.width, 6);
    assert_eq!(always.root.children[0].children[0].rect.width, 5);

    with_scroll_view(&scroll_view, |sv| sv.set_scrollbar(ScrollbarMode::Hidden));
    assert_eq!(render().root.children[0].children[0].rect.width, 6);
    with_scroll_view(&scroll_view, |sv| assert!(!sv.is_scrollbar_visible()));
}

/// layout.test.ts:286-294.
#[test]
fn measures_nested_scroll_content_from_constrained_child_geometry() {
    let inner = shared_component(ScrollView::new(
        text("1\n2\n3\n4\n5\n6"),
        ScrollViewOptions::default(),
    ));
    let outer = shared_component(ScrollView::new(
        shared_component(VStack::new(
            vec![
                entry(
                    inner.clone(),
                    StackEntryOptions {
                        basis: Some(Basis::Fixed(2.0)),
                        ..StackEntryOptions::default()
                    },
                ),
                plain(text("tail")),
            ],
            StackOptions::default(),
        )),
        ScrollViewOptions::default(),
    ));
    render_layout_frame(&outer, 10, 2, noop_render_handle());

    with_scroll_view(&inner, |sv| assert_eq!(sv.viewport_height(), 2));
    with_scroll_view(&outer, |sv| {
        assert_eq!(sv.scroll_by(10), 9);
        assert_eq!(sv.scroll_top(), 1);
    });
}

/// layout.test.ts:296-305.
#[test]
fn rebuilds_geometry_after_content_changes() {
    let (text_component, handle) = shared_lines(vec!["one".into()]);
    let root = shared_component(VStack::new(
        vec![plain(text_component)],
        StackOptions::default(),
    ));
    let first = render_layout_frame(&root, 10, 4, noop_render_handle());
    handle.set_lines(vec!["one".into(), "two".into(), "three".into()]);
    let second = render_layout_frame(&root, 10, 4, noop_render_handle());

    assert_eq!(
        first.root.children[0]
            .lines
            .as_ref()
            .map(|lines| lines.len()),
        Some(1)
    );
    assert_eq!(
        second.root.children[0]
            .lines
            .as_ref()
            .map(|lines| lines.len()),
        Some(3)
    );
}
