//! System tray residency controller (`tray-icon`) enabling 0% GPU window destruction/reopen.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::i18n;

const TRAY_MENU_QUICK_OVERVIEW_ID: &str = "loom.tray.quick_overview";
const TRAY_MENU_SHOW_ID: &str = "loom.tray.show";
const TRAY_MENU_EXIT_ID: &str = "loom.tray.exit";

/// User interaction events emitted by the OS system tray icon or its context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayAction {
    /// Toggle the lightweight live Quick Overview popover window near the system tray.
    QuickOverview,
    /// Restore or reopen the main Loom desktop window.
    Show,
    /// Terminate the Loom application cleanly.
    Exit,
}

#[derive(Default)]
struct SignalState {
    queue: VecDeque<TrayAction>,
    waker: Option<Waker>,
    closed: bool,
}

/// Lock-light waker-backed async signal delivering [`TrayAction`]s directly onto the GPUI loop
/// with zero background threads and zero timer polling.
#[derive(Clone, Default)]
pub(crate) struct TrayEventSignal {
    inner: Arc<Mutex<SignalState>>,
}

impl TrayEventSignal {
    /// Pushes a [`TrayAction`] and wakes any awaiting GPUI foreground task immediately.
    pub(crate) fn push(&self, action: TrayAction) {
        let waker = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            state.queue.push_back(action);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Marks the signal closed so any awaiting loop exits cleanly.
    pub(crate) fn close(&self) {
        let waker = {
            let mut state = self.inner.lock();
            state.closed = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Awaits the next [`TrayAction`], returning `None` once closed.
    pub(crate) fn next(&self) -> NextTrayAction {
        NextTrayAction {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Future returned by [`TrayEventSignal::next`].
pub(crate) struct NextTrayAction {
    inner: Arc<Mutex<SignalState>>,
}

impl Future for NextTrayAction {
    type Output = Option<TrayAction>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if let Some(action) = state.queue.pop_front() {
            return Poll::Ready(Some(action));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Owns the native OS system tray icon and its localized context menu items.
pub(crate) struct TrayController {
    tray_icon: TrayIcon,
    quick_overview_item: MenuItem,
    show_item: MenuItem,
    exit_item: MenuItem,
    signal: TrayEventSignal,
}

impl TrayController {
    /// Creates and registers the OS system tray icon on the main GUI thread.
    pub(crate) fn new() -> Result<(Self, TrayEventSignal), tray_icon::Error> {
        let quick_overview_item = MenuItem::with_id(
            TRAY_MENU_QUICK_OVERVIEW_ID,
            i18n::tr("tray_overview.menu_item"),
            true,
            None,
        );
        let show_item = MenuItem::with_id(TRAY_MENU_SHOW_ID, i18n::tr("tray.show"), true, None);
        let exit_item = MenuItem::with_id(TRAY_MENU_EXIT_ID, i18n::tr("tray.exit"), true, None);

        let menu = Menu::new();
        let _ = menu.append(&quick_overview_item);
        let _ = menu.append(&show_item);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&exit_item);

        let icon = build_embedded_loom_icon();
        let tray_icon = TrayIconBuilder::new()
            .with_tooltip(i18n::tr("tray.tooltip"))
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        let signal = TrayEventSignal::default();

        let click_signal = signal.clone();
        TrayIconEvent::set_event_handler(Some(move |event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                click_signal.push(TrayAction::Show);
            }
            _ => {}
        }));

        let menu_signal = signal.clone();
        MenuEvent::set_event_handler(Some(Arc::new(move |event: MenuEvent| {
            match event.id().0.as_str() {
                TRAY_MENU_QUICK_OVERVIEW_ID => menu_signal.push(TrayAction::QuickOverview),
                TRAY_MENU_SHOW_ID => menu_signal.push(TrayAction::Show),
                TRAY_MENU_EXIT_ID => menu_signal.push(TrayAction::Exit),
                _ => {}
            }
        })));

        Ok((
            Self {
                tray_icon,
                quick_overview_item,
                show_item,
                exit_item,
                signal: signal.clone(),
            },
            signal,
        ))
    }

    /// Re-applies localized strings to the tray tooltip and menu items after a live locale switch.
    pub(crate) fn update_locale(&self) {
        self.quick_overview_item
            .set_text(i18n::tr("tray_overview.menu_item"));
        self.show_item.set_text(i18n::tr("tray.show"));
        self.exit_item.set_text(i18n::tr("tray.exit"));
        let _ = self.tray_icon.set_tooltip(Some(i18n::tr("tray.tooltip")));
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        self.signal.close();
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        MenuEvent::set_event_handler(None);
    }
}

/// Generates a crisp 16x16 RGBA8 programmatic Loom tray emblem (zero external file assets).
fn build_embedded_loom_icon() -> Icon {
    const SIZE: u32 = 16;
    let rgba = render_loom_rgba_16x16();
    Icon::from_rgba(rgba, SIZE, SIZE).expect("embedded 16x16 RGBA icon dimensions are valid")
}

/// Renders a 16x16 RGBA8 geometric Loom badge (rounded deep-slate tile with cyan/teal woven bars).
pub(crate) fn render_loom_rgba_16x16() -> Vec<u8> {
    const SIZE: usize = 16;
    let mut pixels = vec![0u8; SIZE * SIZE * 4];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = (y * SIZE + x) * 4;

            // Round the 4 outer corners of the 16x16 badge.
            let is_corner = matches!(
                (x, y),
                (0, 0)
                    | (1, 0)
                    | (0, 1)
                    | (15, 0)
                    | (14, 0)
                    | (15, 1)
                    | (0, 15)
                    | (1, 15)
                    | (0, 14)
                    | (15, 15)
                    | (14, 15)
                    | (15, 14)
            );
            if is_corner {
                continue;
            }

            // Crisp border frame.
            let is_border = x == 0 || y == 0 || x == SIZE - 1 || y == SIZE - 1;
            if is_border {
                pixels[idx..idx + 4].copy_from_slice(&[38, 99, 138, 255]);
                continue;
            }

            // Stylized "L" + pulse bars representing Loom task monitoring.
            let is_l_stem = (x == 3 || x == 4) && (3..=12).contains(&y);
            let is_l_base = (y == 11 || y == 12) && (3..=11).contains(&x);
            let is_bar_high = (x == 7 || x == 8) && (5..=9).contains(&y);
            let is_bar_tall = (x == 11 || x == 12) && (3..=9).contains(&y);

            if is_l_stem || is_l_base {
                // Bright sky-cyan primary glyph
                pixels[idx..idx + 4].copy_from_slice(&[56, 189, 248, 255]);
            } else if is_bar_high || is_bar_tall {
                // Emerald activity pulse bars
                pixels[idx..idx + 4].copy_from_slice(&[52, 211, 153, 255]);
            } else {
                // Deep slate background
                pixels[idx..idx + 4].copy_from_slice(&[18, 24, 38, 255]);
            }
        }
    }

    pixels
}

#[cfg(test)]
mod tests {
    use super::render_loom_rgba_16x16;

    #[test]
    fn embedded_tray_icon_has_exact_16x16_rgba_dimensions_and_visible_pixels() {
        let rgba = render_loom_rgba_16x16();
        assert_eq!(rgba.len(), 16 * 16 * 4);

        // Top-left corner is transparent (rounded), center pixels are opaque.
        assert_eq!(rgba[3], 0);
        let center_idx = (8 * 16 + 8) * 4;
        assert_eq!(rgba[center_idx + 3], 255);
    }
}
