//! The window: winit for the window and keyboard, softbuffer to paint it,
//! gilrs for gamepads (the owner's choice). The window's event loop owns the
//! main thread, as macOS requires; the program runs on a worker thread and
//! reads the controller the window builds from the keyboard and gamepads.
//! The window paints the frames the program queues to the display, scaled
//! to fit with their shape kept.

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
const BACKGROUND: u32 = 0x0020_2020;

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

/// Paint `frame` into a `width` x `height` buffer: as large as fits with
/// its shape kept, centred, each window pixel taking the frame pixel under
/// its centre (kestrel's choice: sharp pixels, no blending).
fn scale(frame: &Frame, buffer: &mut [u32], width: usize, height: usize) {
    buffer.fill(BACKGROUND);
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if fw == 0 || fh == 0 {
        return;
    }
    let (w, h) = if width * fh <= height * fw {
        (width, (width * fh / fw).max(1))
    } else {
        ((height * fw / fh).max(1), height)
    };
    let (x0, y0) = ((width - w) / 2, (height - h) / 2);
    for y in 0..h {
        let sy = (2 * y + 1) * fh / (2 * h);
        let row = &frame.pixels[sy * fw..(sy + 1) * fw];
        let out = &mut buffer[(y0 + y) * width + x0..][..w];
        for (x, p) in out.iter_mut().enumerate() {
            *p = row[(2 * x + 1) * fw / (2 * w)];
        }
    }
}

struct App {
    title: String,
    config: InputConfig,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
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
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
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
        let made = event_loop
            .create_window(attributes)
            .map_err(|e| e.to_string())
            .and_then(|w| {
                let w = Rc::new(w);
                let context = softbuffer::Context::new(w.clone()).map_err(|e| e.to_string())?;
                let surface =
                    softbuffer::Surface::new(&context, w.clone()).map_err(|e| e.to_string())?;
                Ok((w, surface))
            });
        match made {
            Ok((w, surface)) => {
                self.window = Some(w);
                self.surface = Some(surface);
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
        window: None,
        surface: None,
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
