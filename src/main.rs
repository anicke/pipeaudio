mod library;
mod library_view;
mod recorder;
mod recorder_view;

use std::borrow::Cow;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Sizable, StyledExt, Theme, ThemeMode, TitleBar, h_flex, v_flex,
};
use gpui_kit::*;

use library_view::LibraryView;
use recorder_view::RecorderView;

actions!(pipeaudio, [ToggleRecording]);

// Icons beyond the component library's default set.
assets::icon_assets!(ExtraIcons, [AudioLines, FileMusic, FolderPen, Trash]);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

struct PipeAudio {
    recorder: Entity<RecorderView>,
    library: Entity<LibraryView>,
    focus: FocusHandle,
}

impl PipeAudio {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let library = cx.new(|cx| LibraryView::new(window, cx));
        let recorder = cx.new(|cx| RecorderView::new(library.clone(), cx));
        Self {
            recorder,
            library,
            focus,
        }
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let is_dark = cx.theme().is_dark();
        TitleBar::new()
            .child(
                h_flex()
                    .w_full()
                    .pr_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(assets::IconName::AudioLines)
                                    .small()
                                    .text_color(cx.theme().danger),
                            )
                            .child(div().text_sm().font_semibold().child("PipeAudio")),
                    )
                    .child(
                        Button::new("theme")
                            .ghost()
                            .small()
                            .icon(if is_dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .tooltip(if is_dark { "Light mode" } else { "Dark mode" })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_theme(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for PipeAudio {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PipeAudio")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ToggleRecording, window, cx| {
                this.recorder
                    .update(cx, |recorder, cx| recorder.toggle(window, cx))
            }))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_5()
                    .gap_5()
                    .child(self.recorder.clone())
                    // Cached so the recorder's 30 fps redraws skip the list.
                    .child(
                        self.library
                            .clone()
                            .cached(StyleRefinement::default().flex_1().min_h_0().w_full()),
                    ),
            )
    }
}

fn main() {
    application().with_assets(AppAssets).run(|cx| {
        init(cx);
        cx.bind_keys([KeyBinding::new("space", ToggleRecording, Some("PipeAudio"))]);
        // Closing the window quits; dropping an active Recording finalizes its file.
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(480.), px(760.)), cx)),
            window_min_size: Some(size(px(400.), px(620.))),
            app_id: Some("pipeaudio".into()),
            titlebar: Some(TitlebarOptions {
                title: Some("PipeAudio".into()),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        };
        open_window(options, cx, |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| PipeAudio::new(window, cx))
        })
        .expect("failed to open window");
    });
}
