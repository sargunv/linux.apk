//! What the input and display services share: the window manager's queued
//! window requests, which display connection shows each window, and the
//! input connections that get the seat's lines.
//!
//! The window manager names windows by Android display id; the composer
//! names its HWC displays by its own ids. A request matches the first HWC
//! display of its size that the composer adds after it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use calloop::channel::Sender;

use super::ime;
use super::proto::{WindowPolicy, WindowRequest, event, ty};
use crate::display::wl::Cmd;
use crate::ipc::Subscribers;
use crate::ipc::server::Events;
use crate::ipc::wire::errno::*;

/// How long a request waits for its display.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Where the bridge's own window ids start (DISPLAY_WINDOW).
const OWN_WINDOWS: u32 = 1 << 31;

/// Told where the seat's keyboard focus is: a window, or none of ours.
pub type FocusWatch = Box<dyn Fn(Option<u32>) + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerPosition {
    pub position: (i32, i32),
    pub generation: u64,
}

#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
    clients: Subscribers,
    focus_watch: Mutex<Option<FocusWatch>>,
    window_watch: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    packages: Mutex<HashMap<u32, String>>,
    /// The seat's newest `repeat_info` line, for connections that come later.
    repeat: Mutex<Option<String>>,
    pub evdev: Mutex<Option<super::evdev::Evdev>>,
    pub pointer_positions: Mutex<HashMap<u32, PointerPosition>>,
}

/// The appearance must travel with the request until HWC creates its toplevel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWindow {
    pub request: WindowRequest,
    pub color: u32,
    pub icon: Option<crate::ipc::blob::Image>,
}

#[derive(Default)]
struct State {
    queued: Vec<(PendingWindow, Instant)>,
    /// Window -> the display connection and HWC display showing it.
    shown: HashMap<u32, (u64, u32)>,
    /// Display connections: their Wayland threads.
    displays: HashMap<u64, Sender<Cmd>>,
    next_display: u64,
    /// Windows the bridge names itself (DISPLAY_WINDOW), above any Android
    /// display id.
    next_own: u32,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::default()
    }

    pub fn watch_windows(&self, watch: impl Fn() + Send + Sync + 'static) {
        *self.window_watch.lock().unwrap() = Some(Box::new(watch));
    }

    pub fn open_packages(&self) -> Vec<String> {
        let mut packages: Vec<_> = self.packages.lock().unwrap().values().cloned().collect();
        packages.sort();
        packages.dedup();
        packages
    }

    fn windows_changed(&self) {
        if let Some(watch) = &*self.window_watch.lock().unwrap() {
            watch();
        }
    }

    // ---- the input service's side

    pub fn subscribe(&self, events: Events) -> u64 {
        self.clients.add(events)
    }

    /// What a new connection needs: the newest `repeat_info` line. Locked
    /// with [`Hub::repeat_info`], so a newer line can't go out first.
    pub fn replay(&self, events: &Events) {
        let repeat = self.repeat.lock().unwrap();
        if let Some(l) = &*repeat {
            let _ = events.try_send(ty::INPUT, &event::input(l), &[]);
        }
    }

    pub fn unsubscribe(&self, key: u64) {
        self.clients.remove(key)
    }

    /// Queues a request; one for a queued window replaces it.
    pub fn request(&self, r: WindowRequest) -> Result<(), i32> {
        let mut s = self.state.lock().unwrap();
        if s.shown.contains_key(&r.window) {
            return Err(-EBUSY);
        }
        let now = Instant::now();
        s.queued
            .retain(|(q, at)| q.request.window != r.window && now - *at < REQUEST_TIMEOUT);
        s.queued.push((
            PendingWindow {
                request: r,
                color: 0,
                icon: None,
            },
            now,
        ));
        Ok(())
    }

    /// Activates a shown window, with `token` if not empty.
    pub fn activate(&self, window: u32, token: String) -> Result<(), i32> {
        let s = self.state.lock().unwrap();
        let (conn, _) = s.shown.get(&window).ok_or(-ENOENT)?;
        let token = (!token.is_empty()).then_some(token);
        s.displays[conn]
            .send(Cmd::Activate(window, token))
            .map_err(|_| -EIO)
    }

    pub fn minimize(&self, window: u32) -> Result<(), i32> {
        let s = self.state.lock().unwrap();
        let (conn, _) = s.shown.get(&window).ok_or(-ENOENT)?;
        s.displays[conn]
            .send(Cmd::Minimize(window))
            .map_err(|_| -EIO)
    }

    /// A fresh activation token using the source window and newest seat serial.
    pub fn activation_token(&self, window: u32) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let s = self.state.lock().unwrap();
            let conn = s
                .shown
                .get(&window)
                .map(|(conn, _)| *conn)
                .or_else(|| s.displays.keys().next().copied())?;
            s.displays[&conn].send(Cmd::ExportToken(window, tx)).ok()?;
        }
        rx.recv_timeout(Duration::from_secs(1)).ok()
    }

    pub fn window_appearance(
        &self,
        window: u32,
        color: u32,
        icon: Option<crate::ipc::blob::Image>,
    ) -> Result<(), i32> {
        let mut s = self.state.lock().unwrap();
        if let Some((pending, _)) = s
            .queued
            .iter_mut()
            .find(|(p, _)| p.request.window == window)
        {
            pending.color = color;
            pending.icon = icon;
            return Ok(());
        }
        let (conn, _) = s.shown.get(&window).ok_or(-ENOENT)?;
        s.displays[conn]
            .send(Cmd::WindowAppearance {
                window,
                color,
                icon,
            })
            .map_err(|_| -EIO)
    }

    /// Applies Android's policy to this display's toplevel only.
    pub fn window_policy(&self, policy: WindowPolicy) -> Result<(), i32> {
        let s = self.state.lock().unwrap();
        let (conn, _) = s.shown.get(&policy.window).ok_or(-ENOENT)?;
        s.displays[conn]
            .send(Cmd::WindowPolicy(policy))
            .map_err(|_| -EIO)
    }

    /// Turns this display's toplevel to Android's rotation of its content.
    pub fn window_rotation(&self, window: u32, rotation: u32) -> Result<(), i32> {
        let s = self.state.lock().unwrap();
        let (conn, _) = s.shown.get(&window).ok_or(-ENOENT)?;
        s.displays[conn]
            .send(Cmd::Rotation(window, rotation))
            .map_err(|_| -EIO)
    }

    pub fn keep_awake(&self, window: u32, requested: bool) -> Result<(), i32> {
        let s = self.state.lock().unwrap();
        let (conn, _) = s.shown.get(&window).ok_or(-ENOENT)?;
        s.displays[conn]
            .send(Cmd::KeepAwake { window, requested })
            .map_err(|_| -EIO)
    }

    /// Drops a queued request, or closes a shown window.
    pub fn destroy(&self, window: u32) {
        let mut s = self.state.lock().unwrap();
        s.queued.retain(|(q, _)| q.request.window != window);
        if let Some((conn, _)) = s.shown.remove(&window) {
            let _ = s.displays[&conn].send(Cmd::Destroy(window));
            drop(s);
            self.unmapped(window);
        }
    }

    pub fn pointer_capture(&self, enabled: bool) {
        for d in self.state.lock().unwrap().displays.values() {
            let _ = d.send(Cmd::PointerCapture(enabled));
        }
    }

    /// Goes to every display connection; the one whose seat has text-input
    /// focus applies it.
    pub fn text_input(&self, req: ime::Req) {
        for d in self.state.lock().unwrap().displays.values() {
            let _ = d.send(Cmd::TextInput(req.clone()));
        }
    }

    // ---- the display service's side

    pub fn add_display_connection(&self, cmd: Sender<Cmd>) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.next_display += 1;
        let k = s.next_display;
        s.displays.insert(k, cmd);
        k
    }

    /// The connection is gone, and with it its windows.
    pub fn remove_display_connection(&self, conn: u64) {
        let gone: Vec<u32> = {
            let mut s = self.state.lock().unwrap();
            s.displays.remove(&conn);
            let gone = s
                .shown
                .iter()
                .filter(|(_, (c, _))| *c == conn)
                .map(|(w, _)| *w)
                .collect();
            s.shown.retain(|_, (c, _)| *c != conn);
            gone
        };
        for w in gone {
            self.unmapped(w);
        }
    }

    /// A new window id for a display that brings its own window.
    pub fn own_window(&self) -> u32 {
        let mut s = self.state.lock().unwrap();
        s.next_own = s.next_own.max(OWN_WINDOWS) + 1;
        s.next_own
    }

    /// The request a new HWC display of this size answers, if any.
    pub fn claim(&self, width: u32, height: u32) -> Option<PendingWindow> {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        s.queued.retain(|(_, at)| now - *at < REQUEST_TIMEOUT);
        let i = s
            .queued
            .iter()
            .position(|(q, _)| q.request.width == width && q.request.height == height)?;
        Some(s.queued.remove(i).0)
    }

    /// `display` of connection `conn` now shows the window `r` asked for.
    pub fn mapped(&self, conn: u64, display: u32, r: &WindowRequest) {
        self.state
            .lock()
            .unwrap()
            .shown
            .insert(r.window, (conn, display));
        let prefix = format!("{}.", innkeeper_core::desktop::flatpak_app_id());
        // Android's id uses a lowercase `innkeeper`; the Flatpak id capitalizes it.
        if r.app_id.len() > prefix.len() && r.app_id[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            self.packages
                .lock()
                .unwrap()
                .insert(r.window, r.app_id[prefix.len()..].into());
        }
        self.windows_changed();
        crate::info!("input: window {} shows display {display}", r.window);
    }

    /// The window's first frame is on screen.
    pub fn first_frame(&self, window: u32) {
        let body = event::window_mapped(window);
        self.clients.broadcast(ty::WINDOW_MAPPED, &body, &[], None);
    }

    /// `display` went away. Returns true if it still showed `window`, which
    /// the caller then closes.
    pub fn unmap(&self, conn: u64, display: u32, window: u32) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.shown.get(&window) != Some(&(conn, display)) {
            return false;
        }
        s.shown.remove(&window);
        drop(s);
        self.unmapped(window);
        true
    }

    fn unmapped(&self, window: u32) {
        self.packages.lock().unwrap().remove(&window);
        self.windows_changed();
        let body = event::window_unmapped(window);
        self.clients
            .broadcast(ty::WINDOW_UNMAPPED, &body, &[], None);
    }

    /// The keyboard's repeat rate (per second; 0: no repeat) and delay
    /// (ms), now and for every later connection.
    pub fn repeat_info(&self, rate: i32, delay: i32) {
        let l = format!("repeat_info {rate} {delay}");
        crate::info!("input: key {l}");
        let mut repeat = self.repeat.lock().unwrap();
        self.input(&l);
        *repeat = Some(l);
    }

    pub fn image_released(&self, window: u32, token: u64) {
        self.clients.broadcast(
            ty::IMAGE_RELEASE,
            &event::image_released(window, token),
            &[],
            None,
        );
    }

    /// One input line to every input connection.
    pub fn input(&self, line: &str) {
        self.clients
            .broadcast(ty::INPUT, &event::input(line), &[], None);
        // Keyboard focus also goes to the a11y service.
        if line.starts_with("keyboard_")
            && let Some(watch) = &*self.focus_watch.lock().unwrap()
        {
            if let Some(id) = line.strip_prefix("keyboard_enter ") {
                watch(id.parse().ok());
            } else if line.starts_with("keyboard_leave ") {
                watch(None);
            }
        }
    }

    /// `watch` gets every keyboard focus change.
    pub fn watch_focus(&self, watch: FocusWatch) {
        *self.focus_watch.lock().unwrap() = Some(watch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(window: u32, width: u32) -> WindowRequest {
        WindowRequest {
            window,
            width,
            height: 800,
            dpi: 160,
            min_width: 0,
            min_height: 0,
            app_id: "dev.sargunv.innkeeper.test".into(),
            title: "Test".into(),
            token: String::new(),
        }
    }

    #[test]
    fn a_display_claims_the_request_of_its_size() {
        let hub = Hub::new();
        hub.request(request(5, 1280)).unwrap();
        hub.request(request(6, 1000)).unwrap();
        assert_eq!(hub.claim(1000, 800).map(|r| r.request.window), Some(6));
        assert_eq!(hub.claim(1000, 800), None);
        assert_eq!(hub.claim(1280, 800).map(|r| r.request.window), Some(5));
    }

    #[test]
    fn a_queued_window_accepts_its_initial_icon_before_a_display_claims_it() {
        let hub = Hub::new();
        hub.request(request(5, 1280)).unwrap();
        let icon = crate::ipc::blob::Image {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 255, 255],
        };
        assert_eq!(hub.window_appearance(5, 0xff1976d2, Some(icon)), Ok(()));
    }

    #[test]
    fn initial_appearance_is_claimed_with_the_window_before_it_is_shown() {
        let hub = Hub::new();
        hub.request(request(5, 1280)).unwrap();
        let icon = crate::ipc::blob::Image {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 255, 255],
        };
        hub.window_appearance(5, 0xff1976d2, Some(icon.clone()))
            .unwrap();
        let pending = hub.claim(1280, 800).unwrap();
        assert_eq!(pending.request.window, 5);
        assert_eq!(pending.color, 0xff1976d2);
        assert_eq!(pending.icon, Some(icon));
    }

    #[test]
    fn a_shown_window_is_busy_until_its_display_goes() {
        let hub = Hub::new();
        let (tx, rx) = calloop::channel::channel();
        let conn = hub.add_display_connection(tx);
        hub.request(request(5, 1280)).unwrap();
        let r = hub.claim(1280, 800).unwrap();
        hub.mapped(conn, 2, &r.request);
        assert_eq!(hub.request(request(5, 1280)), Err(-EBUSY));
        hub.activate(5, "t".into()).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Cmd::Activate(5, Some(t))) if t == "t"));
        // Another display of this connection never showed it.
        assert!(!hub.unmap(conn, 3, 5));
        assert!(hub.unmap(conn, 2, 5));
        assert_eq!(hub.activate(5, String::new()), Err(-ENOENT));
        hub.request(request(5, 1280)).unwrap();
    }

    #[test]
    fn destroy_drops_requests_and_closes_windows() {
        let hub = Hub::new();
        let (tx, rx) = calloop::channel::channel();
        let conn = hub.add_display_connection(tx);
        hub.request(request(5, 1280)).unwrap();
        hub.destroy(5);
        assert_eq!(hub.claim(1280, 800), None);
        hub.request(request(6, 1280)).unwrap();
        let r = hub.claim(1280, 800).unwrap();
        hub.mapped(conn, 2, &r.request);
        hub.destroy(6);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Destroy(6))));
        // Its display going away later closes nothing more.
        assert!(!hub.unmap(conn, 2, 6));
    }

    #[test]
    fn a_closed_display_connection_unmaps_its_windows() {
        let hub = Hub::new();
        let (tx, _rx) = calloop::channel::channel();
        let conn = hub.add_display_connection(tx);
        hub.request(request(5, 1280)).unwrap();
        let r = hub.claim(1280, 800).unwrap();
        hub.mapped(conn, 2, &r.request);
        hub.remove_display_connection(conn);
        hub.request(request(5, 1280)).unwrap();
    }

    #[test]
    fn own_windows_never_take_an_android_display_id() {
        let hub = Hub::new();
        let (a, b) = (hub.own_window(), hub.own_window());
        assert!(a > OWN_WINDOWS && b > a);
        // Nothing queued claims them, and Android's requests still match.
        hub.request(request(5, 1280)).unwrap();
        assert_eq!(hub.claim(1280, 800).unwrap().request.window, 5);
    }
}
