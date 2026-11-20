use anyhow::{anyhow, Context, Result};
use evdev::raw_stream::RawDevice;
use evdev::{AbsoluteAxisCode, EventSummary, InputEvent, KeyCode, PropType, SynchronizationCode};
use skyward_config::Write;
use skyward_sys::sleep::Wakeful;
use skyward_ui::app::{Gesture, Key, Touch};
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

pub enum Event {
    /// A touch and when the panel reported it, on [`monotonic_ms`]'s clock.
    Touch(Touch, u64),
    /// A button going down, or coming back up.
    Key(Key, bool),
    /// Several fingers doing one thing together, and when they did it.
    Gesture(Gesture, u64),
}

/// How far down the panel three fingers go before the pull is taken for a
/// gesture, as a share of the panel's height.
const PULL_SHARE: f32 = 0.12;

/// Where one slot's finger is, and where it came down: a panel reports
/// several at once, and what a page reads is one of them.
#[derive(Clone, Copy, Default)]
struct Finger {
    down: bool,
    y: i32,
    came_down_at: i32,
}

/// Milliseconds on the monotonic clock, which the touch panel stamps its
/// events with.
pub fn monotonic_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(rc, 0, "read the monotonic clock");
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

const KEYS: [KeyCode; 4] = [KeyCode::KEY_POWER, KeyCode::KEY_VOLUMEUP, KeyCode::KEY_VOLUMEDOWN, KeyCode::KEY_BACK];

/// How many times a setting is written to a touchscreen whose driver may
/// still be coming back up, and how long to wait between the tries.
const TOUCH_TELLS: usize = 10;
const TOUCH_TELL_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Where the phone's own touch layer keeps the switches of the controller
/// it runs.
const TOUCH_SWITCHES: &str = "/sys/class/touch/touch_dev";

/// Start the touchscreen's controller over after the display rested. One
/// that lost power with the display comes back holding neither the setting
/// it was told nor a reporting mode a page can read, and nothing of the
/// recovery's own init starts it again the way the system's touch layer
/// does. Its power is turned on where it was left off, and a switch that
/// resets it is written and written back.
fn wake_touch() -> Result<()> {
    let Ok(listing) = fs::read_dir(TOUCH_SWITCHES) else { return Ok(()) };
    let mut switches: Vec<(String, PathBuf)> = listing.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
    switches.sort();
    // Which switches the controller has is worth a line of its own: one a
    // build names differently is otherwise only found out by trying.
    let named: Vec<&str> = switches.iter().map(|(name, _)| name.as_str()).collect();
    skyward_sys::kmsg(&format!("the touchscreen's switches: {}", named.join(", ")))?;
    for (name, path) in switches {
        if name.contains("power") {
            let off = fs::read_to_string(&path).is_ok_and(|v| v.trim() == "0");
            if off && fs::write(&path, b"1").is_ok() {
                skyward_sys::kmsg(&format!("the touchscreen is powered on through {}", path.display()))?;
                std::thread::sleep(TOUCH_TELL_WAIT);
            }
        } else if name.contains("reset") && fs::write(&path, b"1").is_ok() {
            std::thread::sleep(TOUCH_TELL_WAIT);
            let _ = fs::write(&path, b"0");
            std::thread::sleep(TOUCH_TELL_WAIT);
            skyward_sys::kmsg(&format!("the touchscreen is reset through {}", path.display()))?;
        }
    }
    Ok(())
}

fn key_of(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::KEY_POWER => Some(Key::Power),
        KeyCode::KEY_VOLUMEUP => Some(Key::VolumeUp),
        KeyCode::KEY_VOLUMEDOWN => Some(Key::VolumeDown),
        KeyCode::KEY_BACK => Some(Key::Back),
        _ => None,
    }
}

/// A device read failed because it was unplugged.
fn gone(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENODEV)
}

/// Slot 0's value of a multitouch axis. The kernel keeps multitouch values
/// per slot, apart from the axis state the ABS queries return, and hands
/// them out through EVIOCGMTSLOTS: the axis code, then one value per slot.
fn slot0_value(dev: &RawDevice, axis: AbsoluteAxisCode, slots: usize) -> io::Result<i32> {
    let mut buf = vec![0i32; 1 + slots];
    buf[0] = i32::from(axis.0);
    // _IOC(_IOC_READ, 'E', 0x0a, size)
    let request = (2u32 << 30) | (((buf.len() * 4) as u32) << 16) | (u32::from(b'E') << 8) | 0x0a;
    if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, buf.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf[1])
}

/// The direct-touch panel. The first finger is followed (multitouch protocol
/// B, slot 0) and given to the page; the other slots are counted, since
/// three fingers pulled down together are a gesture rather than a touch.
struct Panel {
    dev: RawDevice,
    path: PathBuf,
    x_range: (i32, i32),
    y_range: (i32, i32),
    slots: usize,
    slot: i32,
    down: bool,
    was_down: bool,
    raw: (i32, i32),
    moved: bool,
    /// The kernel dropped events that were not read in time; what arrives
    /// up to the next report is incomplete.
    dropped: bool,
    /// Where each slot's finger is, and where it came down.
    fingers: Vec<Finger>,
    /// Whether this pull has already been taken for a gesture: one pull
    /// takes one picture, however long the fingers stay down.
    pulled: bool,
}

impl Panel {
    fn new(path: PathBuf, dev: RawDevice) -> Result<Self> {
        let mut x_range = None;
        let mut y_range = None;
        let mut slots = 1;
        for (axis, info) in dev.get_absinfo().context("read touch axis ranges")? {
            match axis {
                AbsoluteAxisCode::ABS_MT_POSITION_X => x_range = Some((info.minimum(), info.maximum())),
                AbsoluteAxisCode::ABS_MT_POSITION_Y => y_range = Some((info.minimum(), info.maximum())),
                AbsoluteAxisCode::ABS_MT_SLOT => slots = info.maximum() as usize + 1,
                _ => {}
            }
        }
        let x_range = x_range.ok_or_else(|| anyhow!("touch panel reports no X range"))?;
        let y_range = y_range.ok_or_else(|| anyhow!("touch panel reports no Y range"))?;
        // Stamp events with the clock the UI keeps time by: how fast a
        // finger moved when it lifted is measured from these stamps.
        // EVIOCSCLOCKID: _IOW('E', 0xa0, int)
        let request = (1u32 << 30) | (4u32 << 16) | (u32::from(b'E') << 8) | 0xa0;
        let clock: libc::c_int = libc::CLOCK_MONOTONIC;
        if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, &clock) } < 0 {
            return Err(io::Error::last_os_error()).context("stamp touch events with the monotonic clock");
        }
        set_nonblocking(dev.as_raw_fd()).context("make touch non-blocking")?;
        let mut panel = Panel {
            dev,
            path,
            x_range,
            y_range,
            slots,
            slot: 0,
            down: false,
            was_down: false,
            raw: (0, 0),
            moved: false,
            dropped: false,
            fingers: vec![Finger::default(); slots],
            pulled: false,
        };
        // The kernel drops a position equal to the one a slot already holds,
        // so a touch landing where the last one lifted carries no position;
        // start from what the panel holds now.
        panel.resync().context("read the touch state")?;
        eprintln!("touch {:?}: x {:?} y {:?}, {slots} slots, slot 0 at {:?}", panel.dev.name().unwrap_or("?"), x_range, y_range, panel.raw);
        Ok(panel)
    }

    /// Read the current slot and slot 0's finger back from the panel.
    fn resync(&mut self) -> io::Result<()> {
        self.slot = self.dev.get_abs_state()?[usize::from(AbsoluteAxisCode::ABS_MT_SLOT.0)].value;
        self.down = slot0_value(&self.dev, AbsoluteAxisCode::ABS_MT_TRACKING_ID, self.slots)? >= 0;
        self.raw = (
            slot0_value(&self.dev, AbsoluteAxisCode::ABS_MT_POSITION_X, self.slots)?,
            slot0_value(&self.dev, AbsoluteAxisCode::ABS_MT_POSITION_Y, self.slots)?,
        );
        self.moved = true;
        // What the other slots held was dropped, and a pull is taken from
        // where the fingers are now.
        for finger in self.fingers.iter_mut() {
            *finger = Finger::default();
        }
        if let Some(first) = self.fingers.get_mut(0) {
            first.down = self.down;
            first.y = self.raw.1;
            first.came_down_at = self.raw.1;
        }
        Ok(())
    }

    fn read(&mut self, w: f32, h: f32, out: &mut Vec<Event>) -> io::Result<()> {
        let events: Vec<InputEvent> = match self.dev.fetch_events() {
            Ok(events) => events.collect(),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        };
        for e in events {
            match e.destructure() {
                // Skip to the next report and read the state back there.
                EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => self.dropped = true,
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                    if std::mem::take(&mut self.dropped) {
                        self.resync()?;
                    }
                    let time = e.as_ref().time;
                    let at = time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
                    // More than one finger is not a touch on the page: three
                    // pulled down the panel together take a screenshot, and
                    // the fingers of a pull are not handed on as touches.
                    let held = self.fingers.iter().filter(|f| f.down).count();
                    if held >= 2 {
                        if self.was_down {
                            out.push(Event::Touch(Touch::Up, at));
                            self.was_down = false;
                        }
                        if held >= 3
                            && !self.pulled
                            && pulled_down(&self.fingers, self.y_range)
                        {
                            self.pulled = true;
                            out.push(Event::Gesture(Gesture::ThreeFingersDown, at));
                        }
                        self.moved = false;
                        continue;
                    }
                    if held == 0 {
                        self.pulled = false;
                    }
                    let span = |r: (i32, i32)| (r.1 - r.0).max(1) as f32;
                    let x = (self.raw.0 - self.x_range.0) as f32 / span(self.x_range) * w;
                    let y = (self.raw.1 - self.y_range.0) as f32 / span(self.y_range) * h;
                    match (self.was_down, self.down) {
                        (false, true) => out.push(Event::Touch(Touch::Down(x, y), at)),
                        (true, true) if self.moved => out.push(Event::Touch(Touch::Move(x, y), at)),
                        (true, false) => out.push(Event::Touch(Touch::Up, at)),
                        _ => {}
                    }
                    self.was_down = self.down;
                    self.moved = false;
                }
                _ if self.dropped => {}
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_SLOT, v) => self.slot = v,
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_TRACKING_ID, v) => {
                    if let Some(finger) = self.fingers.get_mut(self.slot.max(0) as usize) {
                        finger.down = v >= 0;
                        if v >= 0 {
                            // Where it went down is where it is now until a
                            // position comes for it.
                            finger.came_down_at = finger.y;
                        }
                    }
                    if self.slot == 0 {
                        self.down = v >= 0;
                    }
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_X, v) if self.slot == 0 => {
                    self.raw.0 = v;
                    self.moved = true;
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_MT_POSITION_Y, v) => {
                    if let Some(finger) = self.fingers.get_mut(self.slot.max(0) as usize) {
                        finger.y = v;
                    }
                    if self.slot == 0 {
                        self.raw.1 = v;
                        self.moved = true;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Whether the fingers held went down the panel together, far enough for
/// the pull to be the one a screenshot is taken by.
fn pulled_down(fingers: &[Finger], y_range: (i32, i32)) -> bool {
    let span = (y_range.1 - y_range.0).max(1) as f32;
    fingers.iter().filter(|f| f.down).map(|f| (f.y - f.came_down_at) as f32).fold(f32::MIN, f32::max) > span * PULL_SHARE
}

/// The touchscreen and the hardware buttons. /dev/input is watched, so a
/// device that shows up later, such as a touch panel whose driver module
/// loads after the UI starts, is picked up when it appears.
pub struct Inputs {
    watch: OwnedFd,
    /// The watch and every device taken in: a touch or a key press keeps
    /// the phone awake until it is read.
    wakeful: Wakeful,
    panel: Option<Panel>,
    buttons: Vec<(PathBuf, RawDevice)>,
    seen: HashSet<PathBuf>,
    /// What the touchscreen is told each time its panel is taken in.
    touch_writes: Vec<Write>,
    w: f32,
    h: f32,
}

impl Inputs {
    pub fn open(w: u32, h: u32, touch_writes: Vec<Write>) -> Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("create inotify instance");
        }
        let watch = unsafe { OwnedFd::from_raw_fd(fd) };
        let dir = CString::new("/dev/input")?;
        // IN_ATTRIB too: ueventd sets a node's owner and mode right after
        // creating it, and the node may only open once that is done.
        if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), libc::IN_CREATE | libc::IN_ATTRIB) } < 0 {
            return Err(io::Error::last_os_error()).context("watch /dev/input");
        }
        let wakeful = Wakeful::new()?;
        wakeful.add(fd)?;
        let mut inputs = Inputs {
            watch,
            wakeful,
            panel: None,
            buttons: Vec::new(),
            seen: HashSet::new(),
            touch_writes,
            w: w as f32,
            h: h as f32,
        };
        inputs.scan()?;
        if inputs.panel.is_none() {
            eprintln!("no touch panel yet; waiting for one to appear under /dev/input");
        }
        Ok(inputs)
    }

    /// Take in the devices under /dev/input not seen before: the first
    /// direct multitouch device as the panel, and every device with a
    /// power, volume or back key as buttons. A panel taken in is told what
    /// the device description writes to the touchscreen.
    fn scan(&mut self) -> Result<()> {
        for (path, dev) in evdev::raw_stream::enumerate() {
            if !self.seen.insert(path.clone()) {
                continue;
            }
            let direct = dev.properties().contains(PropType::DIRECT);
            let mt = dev
                .supported_absolute_axes()
                .is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_MT_POSITION_X) && a.contains(AbsoluteAxisCode::ABS_MT_POSITION_Y));
            let keys = dev.supported_keys().is_some_and(|k| KEYS.iter().any(|c| k.contains(*c)));
            eprintln!("input {}: {:?} direct={direct} multitouch={mt} keys={keys}", path.display(), dev.name().unwrap_or("?"));
            if direct && mt && self.panel.is_none() {
                self.wakeful.add(dev.as_raw_fd())?;
                self.panel = Some(Panel::new(path, dev)?);
                self.tell_touch()?;
            } else if keys {
                set_nonblocking(dev.as_raw_fd()).context("make buttons non-blocking")?;
                self.wakeful.add(dev.as_raw_fd())?;
                self.buttons.push((path, dev));
            }
        }
        Ok(())
    }

    /// Empty the watch's queue; true when /dev/input changed.
    fn changed(&self) -> Result<bool> {
        let mut buf = [0u8; 4096];
        let mut any = false;
        loop {
            let n = unsafe { libc::read(self.watch.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                any = true;
                continue;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                ErrorKind::WouldBlock => return Ok(any),
                ErrorKind::Interrupted => continue,
                _ => return Err(err).context("read /dev/input changes"),
            }
        }
    }

    /// Whether a touch panel has come up.
    pub fn has_panel(&self) -> bool {
        self.panel.is_some()
    }

    /// Take the touchscreen in afresh. A panel that comes back on after the
    /// display rested may have had its controller reset under it, with the
    /// setting it was told gone and its frames sent raw again, which no page
    /// reads: opening the device again has its driver start it over, and the
    /// scan tells it what it needs as it is taken in. A device that went with
    /// the display and comes back as a new node is taken in by the watch.
    pub fn reopen_touch(&mut self) -> Result<()> {
        wake_touch()?;
        let Some(panel) = self.panel.take() else {
            return self.tell_touch();
        };
        let path = panel.path.clone();
        // Closing the device takes it out of the watch with it, which is what
        // lets the scan below take it in again.
        drop(panel);
        self.seen.remove(&path);
        self.scan()?;
        if self.panel.is_none() {
            skyward_sys::kmsg(&format!("{} is not opened again; a new node is waited for", path.display()))?;
        }
        Ok(())
    }

    /// Tell the touchscreen what the device's description has it told. It is
    /// told as its panel is taken in, and again each time the panel comes
    /// back on: the rest that turns the display off takes the touch
    /// controller with it, and one the phone's own touch layer is not
    /// running to turn its raw frames into touches comes back sending them,
    /// so nothing moves a page until it is told again. A driver still coming
    /// back up takes the setting a moment later, and the write is tried
    /// until it does.
    pub fn tell_touch(&self) -> Result<()> {
        for w in &self.touch_writes {
            let mut refused = None;
            for attempt in 0..TOUCH_TELLS {
                match fs::write(&w.path, &w.value) {
                    Ok(()) => {
                        refused = None;
                        break;
                    }
                    Err(e) => {
                        refused = Some(e);
                        if attempt + 1 < TOUCH_TELLS {
                            std::thread::sleep(TOUCH_TELL_WAIT);
                        }
                    }
                }
            }
            if let Some(e) = refused {
                return Err(e).with_context(|| format!("write {} to {}", w.value, w.path));
            }
            skyward_sys::kmsg(&format!("wrote {} to {} for the touchscreen", w.value, w.path))?;
        }
        Ok(())
    }

    /// Readable while a device has something queued, or /dev/input changed.
    pub fn fd(&self) -> RawFd {
        self.wakeful.fd()
    }

    /// Everything the devices have queued, as touches and key presses.
    pub fn read(&mut self) -> Result<Vec<Event>> {
        if self.changed()? {
            self.scan()?;
        }
        let mut out = Vec::new();
        if let Some(panel) = &mut self.panel {
            match panel.read(self.w, self.h, &mut out) {
                Ok(()) => {}
                Err(e) if gone(&e) => {
                    eprintln!("input {}: removed", panel.path.display());
                    self.seen.remove(&panel.path);
                    self.panel = None;
                }
                Err(e) => return Err(e).context("read touch events"),
            }
        }
        let mut removed = Vec::new();
        for (i, (path, b)) in self.buttons.iter_mut().enumerate() {
            match b.fetch_events() {
                Ok(events) => {
                    // The kernel's own repeats of a key held are left out;
                    // the UI times its own.
                    for e in events {
                        if let EventSummary::Key(_, code, value @ (0 | 1)) = e.destructure()
                            && let Some(k) = key_of(code)
                        {
                            out.push(Event::Key(k, value == 1));
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if gone(&e) => {
                    eprintln!("input {}: removed", path.display());
                    removed.push(i);
                }
                Err(e) => return Err(e).context("read button events"),
            }
        }
        for i in removed.into_iter().rev() {
            let (path, _) = self.buttons.remove(i);
            self.seen.remove(&path);
        }
        Ok(out)
    }
}
