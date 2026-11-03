//! The window: winit for the window and keyboard, Vulkan (`vulkan.rs`, on
//! Windows and Linux x86_64) or softbuffer to paint it, gilrs for gamepads
//! (the owner's choices). The window's event loop owns the main thread, as
//! macOS requires; the program runs on a worker thread and reads the
//! controller the window builds from the keyboard and gamepads. The window
//! paints the frames the program queues to the display, scaled to fit with
//! their shape kept.

use crate::input_config::InputConfig;
use kestrel_hle::display::{Frame, Screen};
use kestrel_hle::input::{Controller, Pad, Source};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// How often the window reads the gamepads: a little faster than hid
/// samples (every 5 ms), kestrel's choice.
const POLL: Duration = Duration::from_millis(4);
/// The background, and the bars around a frame of another shape: a dark
/// grey.
pub(crate) const BACKGROUND: u32 = 0x0020_2020;

/// How the window paints (`--video`), as Dolphin's video backend setting:
/// Vulkan through the host GPU, or softbuffer on the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Video {
    /// Vulkan where kestrel supports it and the computer has a driver,
    /// softbuffer otherwise.
    Auto,
    Vulkan,
    Software,
}

/// Whether kestrel paints with Vulkan on this platform: Windows and Linux
/// x86_64 for now (PLAN.md section 3, "Platform priority").
pub const VULKAN_HERE: bool = cfg!(all(
    target_arch = "x86_64",
    any(windows, target_os = "linux")
));

impl std::str::FromStr for Video {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Video::Auto),
            "software" => Ok(Video::Software),
            "vulkan" if VULKAN_HERE => Ok(Video::Vulkan),
            "vulkan" => Err("--video vulkan: not yet supported on this platform".into()),
            _ => Err(format!(
                "unknown video backend '{s}' (auto, vulkan, software)"
            )),
        }
    }
}

/// What the program sees: the controller the window last built.
struct SharedPad {
    controller: Controller,
    pad: Arc<Mutex<Pad>>,
}

impl Source for SharedPad {
    fn controller(&self) -> Controller {
        self.controller
    }

    fn sample(&mut self, _: u64) -> Pad {
        *self.pad.lock().unwrap()
    }
}

/// What the program's thread tells the window.
enum Event {
    /// A new frame to paint.
    Frame,
    /// The program finished: close the window.
    Finished,
}

/// Where the program's frames go: the newest waits for the window.
struct SharedScreen {
    frame: Arc<Mutex<Option<Frame>>>,
    proxy: EventLoopProxy<Event>,
}

impl Screen for SharedScreen {
    fn present(&mut self, frame: &Frame) {
        *self.frame.lock().unwrap() = Some(frame.clone());
        let _ = self.proxy.send_event(Event::Frame);
    }
}

/// Where an `fw` x `fh` frame goes in a `width` x `height` window: as
/// large as fits with its shape kept, centred, as (x, y, width, height).
/// None for an empty frame.
pub(crate) fn fit(
    fw: usize,
    fh: usize,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if fw == 0 || fh == 0 {
        return None;
    }
    let (w, h) = if width * fh <= height * fw {
        (width, (width * fh / fw).max(1))
    } else {
        ((height * fw / fh).max(1), height)
    };
    Some(((width - w) / 2, (height - h) / 2, w, h))
}

/// Paint `frame` into a `width` x `height` buffer: as `fit` places it,
/// each window pixel taking the frame pixel under its centre (kestrel's
/// choice: sharp pixels, no blending).
pub(crate) fn scale(frame: &Frame, buffer: &mut [u32], width: usize, height: usize) {
    buffer.fill(BACKGROUND);
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let Some((x0, y0, w, h)) = fit(fw, fh, width, height) else {
        return;
    };
    for y in 0..h {
        let sy = (2 * y + 1) * fh / (2 * h);
        let row = &frame.pixels[sy * fw..(sy + 1) * fw];
        let out = &mut buffer[(y0 + y) * width + x0..][..w];
        for (x, p) in out.iter_mut().enumerate() {
            *p = row[(2 * x + 1) * fw / (2 * w)];
        }
    }
}

/// What paints the window.
enum Painter {
    Software(softbuffer::Surface<Rc<Window>, Rc<Window>>),
    #[cfg(all(target_arch = "x86_64", any(windows, target_os = "linux")))]
    Vulkan(Box<crate::vulkan::Vulkan>),
}

/// softbuffer's painter for `w`.
fn software(w: &Rc<Window>) -> Result<Painter, String> {
    let context = softbuffer::Context::new(w.clone()).map_err(|e| e.to_string())?;
    let surface = softbuffer::Surface::new(&context, w.clone()).map_err(|e| e.to_string())?;
    Ok(Painter::Software(surface))
}

/// The painter `video` asks for: Vulkan when asked or by default where it
/// is supported, falling back to softbuffer with a note when it cannot
/// start (no driver, no GPU that can show the window).
fn painter(video: Video, w: &Rc<Window>) -> Result<Painter, String> {
    #[cfg(all(target_arch = "x86_64", any(windows, target_os = "linux")))]
    if video != Video::Software {
        match crate::vulkan::Vulkan::for_window(w) {
            Ok(v) => {
                eprintln!("note: video: Vulkan on {}", v.name);
                return Ok(Painter::Vulkan(Box::new(v)));
            }
            Err(e) => eprintln!("note: video: Vulkan not usable ({e}); painting with softbuffer"),
        }
    }
    #[cfg(not(all(target_arch = "x86_64", any(windows, target_os = "linux"))))]
    let _ = video;
    software(w)
}

struct App {
    title: String,
    config: InputConfig,
    video: Video,
    window: Option<Rc<Window>>,
    painter: Option<Painter>,
    keys: HashSet<String>,
    gilrs: Option<gilrs::Gilrs>,
    pad: Arc<Mutex<Pad>>,
    frame: Arc<Mutex<Option<Frame>>>,
    interrupt: Arc<AtomicBool>,
    error: Option<String>,
}

impl App {
    /// Build the controller from the keys and every connected gamepad.
    fn update_pad(&mut self) {
        let mut buttons = HashSet::new();
        let mut axes = HashMap::new();
        if let Some(g) = &mut self.gilrs {
            while g.next_event().is_some() {}
            for (_, pad) in g.gamepads() {
                for b in [
                    gilrs::Button::South,
                    gilrs::Button::East,
                    gilrs::Button::North,
                    gilrs::Button::West,
                    gilrs::Button::LeftTrigger,
                    gilrs::Button::LeftTrigger2,
                    gilrs::Button::RightTrigger,
                    gilrs::Button::RightTrigger2,
                    gilrs::Button::Select,
                    gilrs::Button::Start,
                    gilrs::Button::Mode,
                    gilrs::Button::LeftThumb,
                    gilrs::Button::RightThumb,
                    gilrs::Button::DPadUp,
                    gilrs::Button::DPadDown,
                    gilrs::Button::DPadLeft,
                    gilrs::Button::DPadRight,
                ] {
                    if pad.is_pressed(b) {
                        buttons.insert(format!("{b:?}"));
                    }
                }
                for a in [
                    gilrs::Axis::LeftStickX,
                    gilrs::Axis::LeftStickY,
                    gilrs::Axis::RightStickX,
                    gilrs::Axis::RightStickY,
                ] {
                    let v = pad.value(a);
                    let e = axes.entry(format!("{a:?}")).or_insert(0.0f32);
                    // With several gamepads, the one pushed furthest wins.
                    if v.abs() > e.abs() {
                        *e = v;
                    }
                }
            }
        }
        *self.pad.lock().unwrap() = self.config.pad(&self.keys, &buttons, &axes);
    }

    fn paint(&mut self) {
        let (Some(window), Some(painter)) = (&self.window, &mut self.painter) else {
            return;
        };
        #[cfg(all(target_arch = "x86_64", any(windows, target_os = "linux")))]
        if let Painter::Vulkan(v) = painter {
            // The frame is copied out, so the program's thread is not held
            // while the window waits for the display.
            let frame = self.frame.lock().unwrap().clone();
            let size = window.inner_size();
            match v.present(frame.as_ref(), size.width, size.height) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("note: video: Vulkan stopped ({e}); painting with softbuffer");
                    let w = window.clone();
                    match software(&w) {
                        Ok(p) => *painter = p,
                        Err(e) => {
                            self.error = Some(e);
                            self.painter = None;
                            return;
                        }
                    }
                }
            }
        }
        // Where kestrel has no Vulkan the painter has one kind, and this
        // match only unwraps it.
        #[allow(clippy::infallible_destructuring_match)]
        let surface = match painter {
            Painter::Software(surface) => surface,
            #[cfg(all(target_arch = "x86_64", any(windows, target_os = "linux")))]
            Painter::Vulkan(_) => return,
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        if let Ok(mut buffer) = surface.buffer_mut() {
            match &*self.frame.lock().unwrap() {
                Some(frame) => scale(frame, &mut buffer, w.get() as usize, h.get() as usize),
                None => buffer.fill(BACKGROUND),
            }
            let _ = buffer.present();
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let video = self.video;
        let made = event_loop
            .create_window(attributes)
            .map_err(|e| e.to_string())
            .and_then(|w| {
                let w = Rc::new(w);
                let painter = painter(video, &w)?;
                Ok((w, painter))
            });
        match made {
            Ok((w, painter)) => {
                self.window = Some(w);
                self.painter = Some(painter);
            }
            Err(e) => {
                // The program keeps running without a window.
                self.error = Some(e);
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // The program stops at its next slice and the worker then
                // closes the window.
                self.interrupt.store(true, Ordering::Relaxed);
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        ..
                    },
                ..
            } => {
                let name = format!("{code:?}");
                match state {
                    ElementState::Pressed => self.keys.insert(name),
                    ElementState::Released => self.keys.remove(&name),
                };
                self.update_pad();
            }
            // Keys held when the window loses focus never come up.
            WindowEvent::Focused(false) => {
                self.keys.clear();
                self.update_pad();
            }
            WindowEvent::Resized(_) => {
                #[cfg(all(target_arch = "x86_64", any(windows, target_os = "linux")))]
                if let Some(Painter::Vulkan(v)) = &mut self.painter {
                    v.resized();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.paint(),
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Event::Finished => event_loop.exit(),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.update_pad();
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }
}

/// Run `program` on a worker thread with a window feeding it a controller
/// as `config` maps it and showing its frames, until the program stops;
/// then close the window and return what `program` returned. Err when no
/// window can be made here (no display): the caller then runs without one.
pub fn run<R: Send + 'static>(
    title: String,
    config: InputConfig,
    video: Video,
    program: impl FnOnce(Box<dyn Source>, Box<dyn Screen>, Arc<AtomicBool>) -> R + Send + 'static,
) -> Result<R, String> {
    let event_loop = EventLoop::<Event>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    let gilrs = match gilrs::Gilrs::new() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("note: no gamepads: {e}");
            None
        }
    };
    let pad = Arc::new(Mutex::new(Pad::default()));
    let interrupt = Arc::new(AtomicBool::new(false));
    let source = SharedPad {
        controller: config.controller,
        pad: pad.clone(),
    };
    let frame = Arc::new(Mutex::new(None));
    let screen = SharedScreen {
        frame: frame.clone(),
        proxy: event_loop.create_proxy(),
    };
    let proxy = event_loop.create_proxy();
    let flag = interrupt.clone();
    let worker = std::thread::spawn(move || {
        let r = program(Box::new(source), Box::new(screen), flag);
        let _ = proxy.send_event(Event::Finished);
        r
    });
    let mut app = App {
        title,
        config,
        video,
        window: None,
        painter: None,
        keys: HashSet::new(),
        gilrs,
        pad,
        frame,
        interrupt: interrupt.clone(),
        error: None,
    };
    let looped = event_loop.run_app(&mut app);
    if let Some(e) = &app.error {
        eprintln!("note: no window: {e}");
    }
    if let Err(e) = looped {
        eprintln!("note: the window stopped: {e}");
        interrupt.store(true, Ordering::Relaxed);
    }
    worker
        .join()
        .map_err(|_| "the program's thread panicked".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: (0..width * height).collect(),
        }
    }

    #[test]
    fn frames_scale_to_fit_with_their_shape_kept() {
        // 2x1 into 4x4: 4x2, centred, a bar above and below.
        let mut b = vec![0; 16];
        scale(&frame(2, 1), &mut b, 4, 4);
        let g = BACKGROUND;
        assert_eq!(b, [g, g, g, g, 0, 0, 1, 1, 0, 0, 1, 1, g, g, g, g]);
        // 1x2 into 4x2: 1x2, centred, bars left and right.
        let mut b = vec![0; 8];
        scale(&frame(1, 2), &mut b, 4, 2);
        assert_eq!(b, [g, 0, g, g, g, 1, g, g]);
        // 4x4 into 2x2: every other pixel, from each pixel's centre.
        let mut b = vec![0; 4];
        scale(&frame(4, 4), &mut b, 2, 2);
        assert_eq!(b, [5, 7, 13, 15]);
    }
}
