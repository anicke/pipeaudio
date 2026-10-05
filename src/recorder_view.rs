//! The recording panel: start/stop control, timer and level meters.
//!
//! This view is redrawn ~30 times a second while recording, so it is kept
//! separate from the recordings list, which is cached between frames.

use std::time::Duration;

use anyhow::Context as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, StyledExt, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::library::{Entry, format_clock, format_size};
use crate::library_view::LibraryView;
use crate::recorder::{
    BITS_PER_SAMPLE, CHANNELS, HISTORY_LEN, LevelSnapshot, Recording, SAMPLE_RATE,
};

enum State {
    Idle,
    Recording(Recording),
    Saving,
}

pub struct RecorderView {
    state: State,
    /// Where recordings go, and where finished ones are listed.
    library: Entity<LibraryView>,
    _on_quit: Subscription,
}

impl RecorderView {
    pub fn new(library: Entity<LibraryView>, cx: &mut Context<Self>) -> Self {
        // Finish an active recording properly when the app quits.
        let on_quit = cx.on_app_quit(|this, _| {
            let state = std::mem::replace(&mut this.state, State::Idle);
            async move {
                if let State::Recording(recording) = state {
                    let _ = recording.stop();
                }
            }
        });
        Self {
            state: State::Idle,
            library,
            _on_quit: on_quit,
        }
    }

    fn is_recording(&self) -> bool {
        matches!(self.state, State::Recording(_))
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.state {
            State::Idle => self.start(window, cx),
            State::Recording(_) => self.stop(window, cx),
            State::Saving => {}
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match Recording::start(self.library.read(cx).dir()) {
            Ok(recording) => {
                self.state = State::Recording(recording);
                self.library
                    .update(cx, |library, cx| library.set_busy(true, cx));
                // Redraw ~30 times a second for the timer and level meters.
                cx.spawn_in(window, async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(33))
                            .await;
                        let alive = this.update_in(cx, |this, window, cx| {
                            let State::Recording(recording) = &mut this.state else {
                                return false;
                            };
                            if let Some(error) = recording.failure() {
                                window.push_notification(
                                    Notification::error(error).title("Recording stopped"),
                                    cx,
                                );
                                this.stop(window, cx);
                                return false;
                            }
                            cx.notify();
                            true
                        });
                        if !matches!(alive, Ok(true)) {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(error) => window.push_notification(
                Notification::error(format!("{error:#}")).title("Could not start recording"),
                cx,
            ),
        }
        cx.notify();
    }

    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let State::Recording(recording) = std::mem::replace(&mut self.state, State::Saving) else {
            return;
        };
        let finalize = cx.background_spawn(async move {
            let path = recording.stop()?;
            Entry::load(path).context("The recording file is unreadable")
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = finalize.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.state = State::Idle;
                let note = match result {
                    Ok(entry) => {
                        let note =
                            Notification::success(entry.name.clone()).title("Recording saved");
                        this.library
                            .update(cx, |library, cx| library.add(entry, cx));
                        note
                    }
                    Err(error) => {
                        Notification::error(format!("{error:#}")).title("Could not save recording")
                    }
                };
                this.library
                    .update(cx, |library, cx| library.set_busy(false, cx));
                window.push_notification(note, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let record_button = self.render_record_button(cx);
        let theme = cx.theme();
        let recording = self.is_recording();
        let (elapsed, levels) = match &self.state {
            State::Recording(r) => (r.elapsed().as_secs(), Some(r.levels())),
            _ => (0, None),
        };

        let (status, status_color) = match self.state {
            State::Recording(_) => ("Recording", theme.danger),
            State::Saving => ("Saving…", theme.warning),
            State::Idle => ("Ready", theme.success),
        };
        let status_dot = div().size_2().rounded_full().bg(status_color);
        let status_dot = if recording {
            status_dot
                .with_animation("status-pulse", pulse(1200, 0.25, 1.0), |dot, delta| {
                    dot.opacity(delta)
                })
                .into_any_element()
        } else {
            status_dot.into_any_element()
        };

        let details = match &levels {
            Some(levels) => format!(
                "{} kHz · {BITS_PER_SAMPLE}-bit · {} · {}",
                SAMPLE_RATE / 1000,
                if CHANNELS == 2 { "Stereo" } else { "Mono" },
                format_size(levels.bytes_written)
            ),
            None => "Captures everything playing on your desktop".to_string(),
        };

        v_flex()
            .w_full()
            .items_center()
            .gap_4()
            .p_6()
            .rounded(theme.radius_lg * 1.5)
            .border_1()
            .border_color(if recording {
                theme.danger.opacity(0.4)
            } else {
                theme.border
            })
            .bg(theme.secondary.opacity(0.5))
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(status_color.opacity(0.12))
                    .text_xs()
                    .font_medium()
                    .text_color(status_color)
                    .child(status_dot)
                    .child(status),
            )
            .child(
                div()
                    .text_size(px(52.))
                    .font_family(theme.mono_font_family.clone())
                    .line_height(px(60.))
                    .text_color(if recording {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(format_clock(elapsed)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(details),
            )
            .child(self.render_waveform(levels.as_ref(), cx))
            .child(self.render_meters(levels.as_ref(), cx))
            .child(record_button)
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Press")
                    .child(
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .font_family(theme.mono_font_family.clone())
                            .child("Space"),
                    )
                    .child(if recording {
                        "to stop"
                    } else {
                        "to start recording"
                    }),
            )
            .into_any_element()
    }

    fn render_waveform(&self, levels: Option<&LevelSnapshot>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let history = levels.map(|l| l.history.as_slice()).unwrap_or(&[]);
        let padding = HISTORY_LEN.saturating_sub(history.len());
        let color = if levels.is_some() {
            theme.danger
        } else {
            theme.muted_foreground.opacity(0.3)
        };
        let max_height = 56.;

        h_flex()
            .w_full()
            .h(px(max_height))
            .gap(px(2.))
            .items_center()
            .children(
                std::iter::repeat_n(0.0, padding)
                    .chain(history.iter().copied())
                    .enumerate()
                    .map(|(i, level)| {
                        // Fade older bars out towards the left edge.
                        let age = i as f32 / HISTORY_LEN as f32;
                        div()
                            .flex_1()
                            .h(px((loudness(level) * max_height).max(3.)))
                            .rounded_full()
                            .bg(color.opacity(0.25 + 0.75 * age))
                    }),
            )
            .into_any_element()
    }

    fn render_meters(&self, levels: Option<&LevelSnapshot>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let peak = levels.map_or([0.; 2], |l| l.peak);
        v_flex()
            .w_full()
            .gap_1p5()
            .children(["L", "R"].into_iter().zip(peak).map(|(label, level)| {
                let fill = loudness(level);
                let color = if level >= 0.98 {
                    theme.danger
                } else if fill > 0.8 {
                    theme.warning
                } else {
                    theme.success
                };
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().w_3().child(label))
                    .child(
                        div()
                            .flex_1()
                            .h_1p5()
                            .rounded_full()
                            .bg(theme.muted)
                            .child(div().h_full().rounded_full().bg(color).w(relative(fill))),
                    )
            }))
            .into_any_element()
    }

    fn render_record_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let recording = self.is_recording();
        let saving = matches!(self.state, State::Saving);
        let red = theme.danger;

        let inner = if recording {
            div().size(px(30.)).rounded(px(7.)).bg(red)
        } else {
            div()
                .size(px(58.))
                .rounded_full()
                .bg(red)
                .when(saving, |d| d.opacity(0.4))
        };

        let button = div()
            .id("record")
            .size(px(84.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_4()
            .border_color(if recording {
                red.opacity(0.5)
            } else {
                theme.border
            })
            .bg(theme.background)
            .shadow_md()
            .when(!saving, |b| {
                b.cursor_pointer()
                    .hover(|s| s.border_color(red.opacity(0.6)))
                    .active(|s| s.opacity(0.85))
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(if recording {
                    "Stop recording"
                } else {
                    "Start recording"
                })
                .build(window, cx)
            })
            .child(inner);

        if recording {
            button
                .with_animation("record-pulse", pulse(1400, 0.2, 0.8), move |b, delta| {
                    b.border_color(red.opacity(delta))
                })
                .into_any_element()
        } else {
            button.into_any_element()
        }
    }
}

impl Render for RecorderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(cx)
    }
}

/// A repeating pulse between `low` and `high`, for things that throb while recording.
fn pulse(millis: u64, low: f32, high: f32) -> Animation {
    Animation::new(Duration::from_millis(millis))
        .repeat()
        .with_easing(pulsating_between(low, high))
}

/// Maps a linear peak (0..=1) onto a -60 dB..0 dB display scale.
fn loudness(level: f32) -> f32 {
    if level <= 0.0 {
        return 0.0;
    }
    ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}
