//! The window: winit for the window and keyboard, softbuffer to paint it,
//! gilrs for gamepads (the owner's choice). The window's event loop owns the
//! main thread, as macOS requires; the program runs on a worker thread and
//! reads the controller the window builds from the keyboard and gamepads.
//! Nothing is drawn yet but a plain background: graphics come in phase 5.

use crate::input_config::InputConfig;
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
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// How often the window reads the gamepads: a little faster than hid
/// samples (every 5 ms), kestrel's choice.
const POLL: Duration = Duration::from_millis(4);
/// The background: a dark grey.
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

/// The program finished: close the window.
struct Finished;

struct App {
    title: String,
    config: InputConfig,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    keys: HashSet<String>,
    gilrs: Option<gilrs::Gilrs>,
    pad: Arc<Mutex<Pad>>,
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
            buffer.fill(BACKGROUND);
            let _ = buffer.present();
        }
    }
}

impl ApplicationHandler<Finished> for App {
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: Finished) {
        event_loop.exit();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.update_pad();
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }
}

/// Run `program` on a worker thread with a window feeding it a controller
/// as `config` maps it, until the program stops; then close the window and
/// return what `program` returned. Err when no window can be made here (no
/// display): the caller then runs without one.
pub fn run<R: Send + 'static>(
    title: String,
    config: InputConfig,
    program: impl FnOnce(Box<dyn Source>, Arc<AtomicBool>) -> R + Send + 'static,
) -> Result<R, String> {
    let event_loop = EventLoop::<Finished>::with_user_event()
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
    let proxy = event_loop.create_proxy();
    let flag = interrupt.clone();
    let worker = std::thread::spawn(move || {
        let r = program(Box::new(source), flag);
        let _ = proxy.send_event(Finished);
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
