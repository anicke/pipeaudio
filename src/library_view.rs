//! The recordings in the output folder, and controls for choosing it.

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, InteractiveElementExt, Sizable, StyledExt, WindowExt,
    h_flex, v_flex,
};
use gpui_kit::*;

use crate::library::{self, Entry, format_clock, format_size, format_when};
use crate::recorder;

pub struct LibraryView {
    dir: PathBuf,
    entries: Vec<Entry>,
    /// The folder can't be changed while a recording is in progress.
    busy: bool,
    /// The folder scan in flight; replacing it cancels a stale one.
    _scan: Task<()>,
}

impl LibraryView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            dir: PathBuf::new(),
            entries: Vec::new(),
            busy: false,
            _scan: Task::ready(()),
        };
        this.set_dir(library::load_dir(), window, cx);
        this
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    /// Lists a newly finished recording at the top.
    pub fn add(&mut self, entry: Entry, cx: &mut Context<Self>) {
        self.entries.retain(|existing| existing.path != entry.path);
        self.entries.insert(0, entry);
        cx.notify();
    }

    /// Switches to `dir`, finishing any recordings a crash left behind in it.
    fn set_dir(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Recovery is quick and must finish before a new recording can start
        // in `dir`; reading every file's duration is slow, so it runs off-thread.
        let recovered = recorder::recover(&dir).len();
        let scan = cx.background_spawn({
            let dir = dir.clone();
            async move { library::scan(&dir) }
        });
        self.dir = dir;
        self.entries.clear();
        cx.notify();
        self._scan = cx.spawn_in(window, async move |this, cx| {
            let entries = scan.await;
            let _ = this.update_in(cx, |this, window, cx| {
                // Pushed from here because at startup the window's
                // notification layer doesn't exist until this view is built.
                if recovered > 0 {
                    let note = format!("Recovered {recovered} unfinished recording(s)");
                    window.push_notification(Notification::info(note), cx);
                }
                this.entries = entries;
                cx.notify();
            });
        });
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose recordings folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                library::save_dir(&dir);
                this.set_dir(dir, window, cx);
            });
        })
        .detach();
    }

    fn delete(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let name = entry.name.clone();
        let path = entry.path.clone();
        let trashed = cx.background_spawn({
            let path = path.clone();
            async move { library::trash(&path) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let trashed = trashed.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let note = if trashed {
                    this.entries.retain(|entry| entry.path != path);
                    Notification::info(name).title("Moved to Trash")
                } else {
                    Notification::error(name).title("Could not move to Trash")
                };
                window.push_notification(note, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<_> = (self.entries.iter().enumerate())
            .map(|(ix, entry)| self.render_entry(ix, entry, cx).into_any_element())
            .collect();
        let theme = cx.theme();
        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_semibold().child("Recordings"))
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_md()
                                    .bg(theme.muted)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(self.entries.len().to_string()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("change-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(assets::IconName::FolderPen)
                                    .tooltip("Change folder")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_folder(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("open-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::FolderOpen)
                                    .tooltip("Open folder")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let _ = std::fs::create_dir_all(&this.dir);
                                        cx.open_with_system(&this.dir);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(self.dir.display().to_string()),
            )
            .child(if self.entries.is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(assets::IconName::FileMusic).large())
                    .child(div().text_sm().child("No recordings yet"))
                    .into_any_element()
            } else {
                v_flex()
                    .id("recordings")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_1()
                    .children(rows)
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_entry(&self, ix: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let meta: Vec<_> = (entry.duration_secs.map(format_clock).into_iter())
            .chain([format_when(entry.modified), format_size(entry.size)])
            .collect();
        let path = entry.path.clone();

        h_flex()
            .id(ix)
            .gap_3()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.secondary))
            .on_double_click({
                let path = path.clone();
                move |_, _, cx| cx.open_with_system(&path)
            })
            .child(
                div()
                    .size_8()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.radius)
                    .bg(theme.danger.opacity(0.12))
                    .text_color(theme.danger)
                    .child(Icon::new(assets::IconName::AudioLines).small()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(entry.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(meta.join(" · ")),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("play-{ix}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Play)
                    .tooltip("Play")
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            )
            .child(
                Button::new(SharedString::from(format!("trash-{ix}")))
                    .ghost()
                    .xsmall()
                    .icon(assets::IconName::Trash)
                    .tooltip("Move to Trash")
                    .on_click(cx.listener(move |this, _, window, cx| this.delete(ix, window, cx))),
            )
            .into_any_element()
    }
}

impl Render for LibraryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_list(cx)
    }
}
