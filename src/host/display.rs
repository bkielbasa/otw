//! Optional host-side window system for the emulated Win95 guest.
//!
//! Enabled by the `display` Cargo feature. Two entry points are provided:
//! * [`show_frame`] — static viewer for captured boot-sector frames.
//! * [`GuiRuntime`] — live window system driven by the guest's own
//!   `CreateWindowExA` / `GetMessageA` / `DispatchMessageA` calls.
//!
//! Both are excluded from default builds so the packaged static-pie binary
//! does not link against X11 or OpenGL.

use std::{cell::RefCell, fmt, rc::Rc, time::Duration};

use glow::HasContext;
use glutin::{
    config::ConfigTemplateBuilder,
    context::{
        ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext, Version,
    },
    display::{GetGlDisplay, GlDisplay},
    prelude::GlSurface,
    surface::{Surface, SurfaceAttributesBuilder, WindowSurface},
};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    platform::pump_events::EventLoopExtPumpEvents,
    raw_window_handle::HasWindowHandle,
    window::{CursorGrabMode, Window, WindowAttributes},
};

use crate::{
    fat32::Fat32FileSystem,
    host::font::FONT_DATA,
    machine::VideoFrame,
    session::Session,
    shell::launch::DesktopShortcut,
    shell::{
        setup,
        volume::{parse_path, Volume},
        Button as ShellButton, Key as ShellKey, Layer, Mods, PointerOutcome, Shell, Task,
        TrayEvent, TrayItem,
    },
    win32::{GuestMessage, GuiState, SharedState},
};

const STATIC_TITLE: &str = "OTW VM — guest framebuffer";

// Virtual desktop geometry used by the live Win32 GUI. The host window is the
// desktop; guest top-level windows are drawn inside it and can be dragged.
const DESKTOP_WIDTH: u32 = 1024;
const DESKTOP_HEIGHT: u32 = 700;
const WINDOW_BORDER: i32 = 3;
const TITLE_HEIGHT: i32 = 22;
const MENU_HEIGHT: i32 = 26;
const CLOSE_BUTTON: i32 = 16;
const RESIZE_HANDLE: i32 = 16;
const MIN_WINDOW_WIDTH: i32 = 240;
const MIN_WINDOW_HEIGHT: i32 = 180;
use crate::shell::TASKBAR_HEIGHT;
const PUMP_INTERVAL: Duration = Duration::from_millis(16);

/// Failures from the host window/GL backend.
#[derive(Debug)]
pub enum DisplayError {
    EventLoop(String),
    Gl(String),
    Shader(String),
    InvalidFrame(String),
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(message) => write!(f, "host event loop error: {message}"),
            Self::Gl(message) => write!(f, "host OpenGL error: {message}"),
            Self::Shader(message) => write!(f, "host shader error: {message}"),
            Self::InvalidFrame(message) => write!(f, "invalid video frame: {message}"),
        }
    }
}

impl std::error::Error for DisplayError {}

// ── Static frame viewer ────────────────────────────────────────────────────

/// Open a window and display `frame` until the user closes it or presses Escape.
pub fn show_frame(frame: VideoFrame) -> Result<(), DisplayError> {
    validate_frame(&frame)?;
    let event_loop =
        EventLoop::new().map_err(|error| DisplayError::EventLoop(error.to_string()))?;
    let mut app = StaticApp::new(frame);
    event_loop
        .run_app(&mut app)
        .map_err(|error| DisplayError::EventLoop(error.to_string()))?;
    match app.error.take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn validate_frame(frame: &VideoFrame) -> Result<(), DisplayError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(DisplayError::InvalidFrame(
            "width and height must be non-zero".to_string(),
        ));
    }
    let expected = frame
        .width
        .checked_mul(frame.height)
        .and_then(|pixels| pixels.checked_mul(3));
    if expected != Some(frame.pixels.len()) {
        return Err(DisplayError::InvalidFrame(format!(
            "expected {} RGB bytes, got {}",
            expected.unwrap_or(usize::MAX),
            frame.pixels.len()
        )));
    }
    Ok(())
}

fn window_size(frame: &VideoFrame) -> (u32, u32) {
    let scale = (1..=4)
        .rev()
        .find(|&scale| {
            frame.width * scale as usize <= 1600 && frame.height * scale as usize <= 1000
        })
        .unwrap_or(1);
    ((frame.width * scale) as u32, (frame.height * scale) as u32)
}

// ── GL resources ───────────────────────────────────────────────────────────

struct GlState {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    texture: glow::Texture,
}

impl GlState {
    fn new(gl: glow::Context, frame: &VideoFrame) -> Result<Self, DisplayError> {
        let version = unsafe { gl.get_parameter_string(glow::VERSION) };
        let (vertex_source, fragment_source) = if version.to_ascii_lowercase().contains("opengl es")
        {
            (
                "attribute vec2 a_pos;\n\
                 attribute vec2 a_uv;\n\
                 varying vec2 v_uv;\n\
                 void main() {\n\
                     v_uv = a_uv;\n\
                     gl_Position = vec4(a_pos, 0.0, 1.0);\n\
                 }\n",
                "precision mediump float;\n\
                 varying vec2 v_uv;\n\
                 uniform sampler2D u_tex;\n\
                 void main() {\n\
                     gl_FragColor = texture2D(u_tex, v_uv);\n\
                 }\n",
            )
        } else {
            (
                "#version 330 core\n\
                 in vec2 a_pos;\n\
                 in vec2 a_uv;\n\
                 out vec2 v_uv;\n\
                 void main() {\n\
                     v_uv = a_uv;\n\
                     gl_Position = vec4(a_pos, 0.0, 1.0);\n\
                 }\n",
                "#version 330 core\n\
                 in vec2 v_uv;\n\
                 out vec4 out_color;\n\
                 uniform sampler2D u_tex;\n\
                 void main() {\n\
                     out_color = texture(u_tex, v_uv);\n\
                 }\n",
            )
        };

        let vertex = compile_shader(&gl, glow::VERTEX_SHADER, vertex_source)?;
        let fragment = compile_shader(&gl, glow::FRAGMENT_SHADER, fragment_source)?;
        let program = unsafe { gl.create_program() }
            .map_err(|error| DisplayError::Gl(format!("creating program: {error}")))?;
        unsafe {
            gl.attach_shader(program, vertex);
            gl.attach_shader(program, fragment);
            gl.link_program(program);
        }
        let linked = unsafe { gl.get_program_link_status(program) };
        let link_log = unsafe { gl.get_program_info_log(program) };
        unsafe {
            gl.delete_shader(vertex);
            gl.delete_shader(fragment);
        }
        if !linked {
            return Err(DisplayError::Shader(format!("linking program: {link_log}")));
        }

        let vao = unsafe { gl.create_vertex_array() }
            .map_err(|error| DisplayError::Gl(format!("creating VAO: {error}")))?;
        let vbo = unsafe { gl.create_buffer() }
            .map_err(|error| DisplayError::Gl(format!("creating VBO: {error}")))?;
        let texture = unsafe { gl.create_texture() }
            .map_err(|error| DisplayError::Gl(format!("creating texture: {error}")))?;

        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0,
        ];
        let vertex_bytes: Vec<u8> = vertices
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect();

        unsafe {
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &vertex_bytes, glow::STATIC_DRAW);

            let position = gl
                .get_attrib_location(program, "a_pos")
                .ok_or_else(|| DisplayError::Shader("missing a_pos attribute".to_string()))?;
            let uv = gl
                .get_attrib_location(program, "a_uv")
                .ok_or_else(|| DisplayError::Shader("missing a_uv attribute".to_string()))?;
            let stride = 4 * std::mem::size_of::<f32>() as i32;
            gl.enable_vertex_attrib_array(position);
            gl.vertex_attrib_pointer_f32(position, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(uv);
            gl.vertex_attrib_pointer_f32(
                uv,
                2,
                glow::FLOAT,
                false,
                stride,
                2 * std::mem::size_of::<f32>() as i32,
            );

            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            upload(&gl, texture, frame);

            gl.use_program(Some(program));
            let sampler = gl.get_uniform_location(program, "u_tex");
            gl.uniform_1_i32(sampler.as_ref(), 0);

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }

        Ok(Self {
            gl,
            program,
            vao,
            vbo,
            texture,
        })
    }

    fn update(&self, frame: &VideoFrame) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            upload(&self.gl, self.texture, frame);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    fn draw(&self, width: u32, height: u32) {
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width as i32, height as i32);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
        }
    }
}

fn upload(gl: &glow::Context, texture: glow::Texture, frame: &VideoFrame) {
    unsafe {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGB as i32,
            frame.width as i32,
            frame.height as i32,
            0,
            glow::RGB,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&frame.pixels)),
        );
    }
    let _ = texture;
}

impl Drop for GlState {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_texture(self.texture);
            self.gl.delete_program(self.program);
        }
    }
}

fn compile_shader(
    gl: &glow::Context,
    kind: u32,
    source: &str,
) -> Result<glow::Shader, DisplayError> {
    let shader = unsafe { gl.create_shader(kind) }
        .map_err(|error| DisplayError::Gl(format!("creating shader: {error}")))?;
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if unsafe { gl.get_shader_compile_status(shader) } {
        Ok(shader)
    } else {
        let log = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        Err(DisplayError::Shader(format!("compiling shader: {log}")))
    }
}

fn gl_context(
    event_loop: &ActiveEventLoop,
    attributes: WindowAttributes,
) -> Result<
    (
        Window,
        glutin::config::Config,
        glutin::display::Display,
        glutin::context::NotCurrentContext,
    ),
    DisplayError,
> {
    let config_template = ConfigTemplateBuilder::new()
        .with_depth_size(0)
        .with_stencil_size(0)
        .with_transparency(false);
    let (mut window, gl_config) = glutin_winit::DisplayBuilder::new()
        .with_window_attributes(Some(attributes.clone()))
        .build(event_loop, config_template, |mut configs| {
            configs.next().expect("no compatible OpenGL configuration")
        })
        .map_err(|error| DisplayError::Gl(format!("creating GL config: {error}")))?;
    let gl_display = gl_config.display();
    let raw_handle = window
        .as_ref()
        .map(|window| window.window_handle().expect("window handle").as_raw());
    let context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
        .build(raw_handle);
    let fallback_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
        .build(raw_handle);
    let not_current = unsafe {
        gl_display
            .create_context(&gl_config, &context_attributes)
            .or_else(|_| gl_display.create_context(&gl_config, &fallback_attributes))
            .map_err(|error| DisplayError::Gl(format!("creating GL context: {error}")))?
    };
    let window = window.take().unwrap_or_else(|| {
        glutin_winit::finalize_window(event_loop, attributes, &gl_config)
            .expect("finalizing GL window")
    });
    Ok((window, gl_config, gl_display, not_current))
}

fn make_current(
    window: &Window,
    gl_config: &glutin::config::Config,
    gl_display: &glutin::display::Display,
    not_current: glutin::context::NotCurrentContext,
) -> Result<
    (
        PossiblyCurrentContext,
        Surface<WindowSurface>,
        glow::Context,
    ),
    DisplayError,
> {
    let inner = window.inner_size();
    let surface_width = NonZeroU32::new(inner.width).unwrap_or(NonZeroU32::MIN);
    let surface_height = NonZeroU32::new(inner.height).unwrap_or(NonZeroU32::MIN);
    let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        window.window_handle().unwrap().as_raw(),
        surface_width,
        surface_height,
    );
    let gl_surface = unsafe {
        gl_display
            .create_window_surface(gl_config, &surface_attributes)
            .map_err(|error| DisplayError::Gl(format!("creating GL surface: {error}")))?
    };
    let gl_context = not_current
        .make_current(&gl_surface)
        .map_err(|error| DisplayError::Gl(format!("making GL context current: {error}")))?;
    let _ = gl_surface.set_swap_interval(
        &gl_context,
        glutin::surface::SwapInterval::Wait(NonZeroU32::MIN),
    );
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            let name = std::ffi::CString::new(name).expect("GL function name");
            gl_display.get_proc_address(&name)
        })
    };
    Ok((gl_context, gl_surface, gl))
}

use std::num::NonZeroU32;

// ── Static app ─────────────────────────────────────────────────────────────

struct StaticApp {
    frame: VideoFrame,
    window: Option<Window>,
    gl_state: Option<GlState>,
    gl_surface: Option<Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    error: Option<DisplayError>,
}

impl StaticApp {
    fn new(frame: VideoFrame) -> Self {
        Self {
            frame,
            window: None,
            gl_state: None,
            gl_surface: None,
            gl_context: None,
            error: None,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: DisplayError) {
        self.error = Some(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for StaticApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.error.is_some() {
            return;
        }
        let (width, height) = window_size(&self.frame);
        let attributes = WindowAttributes::default()
            .with_title(STATIC_TITLE)
            .with_inner_size(winit::dpi::LogicalSize { width, height })
            .with_resizable(true);
        let result = (|| -> Result<(), DisplayError> {
            let (window, config, display, not_current) = gl_context(event_loop, attributes)?;
            let (context, surface, gl) = make_current(&window, &config, &display, not_current)?;
            let state = GlState::new(gl, &self.frame)?;
            self.window = Some(window);
            self.gl_state = Some(state);
            self.gl_surface = Some(surface);
            self.gl_context = Some(context);
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                event_loop.exit()
            }
            WindowEvent::Resized(size) => {
                if let (Some(context), Some(surface)) = (&self.gl_context, &self.gl_surface) {
                    if let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        let _ = surface.resize(context, width, height);
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(state) = &self.gl_state {
                    let size = self
                        .window
                        .as_ref()
                        .map(|window| window.inner_size())
                        .map(|size| (size.width, size.height))
                        .unwrap_or((self.frame.width as u32, self.frame.height as u32));
                    state.draw(size.0, size.1);
                    if let (Some(context), Some(surface)) = (&self.gl_context, &self.gl_surface) {
                        let _ = surface.swap_buffers(context);
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Live guest GUI ─────────────────────────────────────────────────────────

/// Host window system driven by the emulated guest. Pump it from the CPU
/// backend before blocking message APIs; it translates OS events into guest
/// `MSG`s and renders the guest's windows.
pub struct GuiRuntime {
    host: Rc<RefCell<Host>>,
    session: Rc<Session>,
}

/// The host window and its event loop. Shared with the session's programs,
/// which pump it while they wait in modal loops.
struct Host {
    event_loop: EventLoop<()>,
    app: GuestApp,
}

impl GuiRuntime {
    pub fn new(shared: Rc<RefCell<SharedState>>) -> Result<Self, DisplayError> {
        Self::new_with_explorer(shared, None, false, Vec::new())
    }

    /// Create the desktop. The shell browses `explorer_fs` (a FAT32 disk
    /// image, read-only) when given, otherwise drive C:. `shell_only` is the
    /// desktop session (`--desktop`): drive C: is set up on first run and the
    /// shell owns the host window's lifetime. `programs` become desktop icons
    /// (for disk images, which have no desktop folder).
    pub fn new_with_explorer(
        shared: Rc<RefCell<SharedState>>,
        explorer_fs: Option<Rc<Fat32FileSystem>>,
        shell_only: bool,
        programs: Vec<DesktopShortcut>,
    ) -> Result<Self, DisplayError> {
        let event_loop =
            EventLoop::new().map_err(|error| DisplayError::EventLoop(error.to_string()))?;
        // `pump_app_events` uses winit's control flow to decide whether to block.
        // The default `Poll` makes the shell-only desktop spin at 100% CPU while
        // idle; `Wait` lets `pump(timeout)` sleep until the next event instead.
        event_loop.set_control_flow(ControlFlow::Wait);
        let volume = match explorer_fs {
            Some(fs) => Volume::Image(fs),
            None => {
                let volume = Volume::drive_c();
                if shell_only {
                    if let Err(error) = setup::install(&volume, setup::default_source().as_deref())
                    {
                        eprintln!("[shell] setting up drive C: failed: {error}");
                    }
                }
                volume
            }
        };
        let programs = programs
            .iter()
            .map(|program| (program.name.clone(), parse_path(&program.path)))
            .collect();
        let shell = Shell::new(Some(volume), programs);
        let session = Session::new();
        let app = GuestApp::new_with_shell(shared, shell, shell_only, session.clone());
        let host = Rc::new(RefCell::new(Host { event_loop, app }));
        // Programs waiting in modal loops keep the desktop responsive.
        let weak = Rc::downgrade(&host);
        session.set_host_pump(Box::new(move |block| {
            if let Some(host) = weak.upgrade() {
                if let Ok(mut host) = host.try_borrow_mut() {
                    host.pump(if block { PUMP_INTERVAL } else { Duration::ZERO });
                }
            }
        }));
        let runtime = Self { host, session };
        // Deliver the initial Resumed/NewEvents so the event loop is live.
        {
            let mut host = runtime.host.borrow_mut();
            let Host { event_loop, app } = &mut *host;
            event_loop.pump_app_events(Some(Duration::ZERO), app);
        }
        Ok(runtime)
    }

    /// The desktop session (programs other than the one driving the
    /// display).
    pub fn session(&self) -> Rc<Session> {
        self.session.clone()
    }

    /// Pump host events for at most `timeout`. Guest messages produced by the
    /// events are appended to `GuiState::queue` for `GetMessageA` to consume.
    pub fn pump(&mut self, timeout: Duration) {
        self.host.borrow_mut().pump(timeout);
    }

    /// Run the desktop until it is shut down: host events, then a turn for
    /// every running program.
    pub fn pump_until_closed(&mut self) {
        loop {
            {
                let host = self.host.borrow();
                if host.app.window.is_none() || host.app.error.is_some() {
                    break;
                }
            }
            let idle = self.session.is_empty();
            self.host
                .borrow_mut()
                .pump(if idle { PUMP_INTERVAL } else { Duration::ZERO });
            // Winit's X11 pump polls the display socket for both read and
            // write, so it can return immediately even with `Wait` control
            // flow. Sleep explicitly to keep the idle desktop from spinning
            // at full CPU and starving the rest of the host session.
            std::thread::sleep(if idle {
                PUMP_INTERVAL
            } else {
                Duration::from_millis(4)
            });
            self.session.run_programs();
            let mut host = self.host.borrow_mut();
            host.after_programs();
            if host.app.shell.quit {
                host.app.window = None;
            }
        }
    }
}

impl Host {
    fn pump(&mut self, timeout: Duration) {
        // A display runtime always owns the desktop window. The guest may not
        // have created any windows yet, and desktop shortcuts must remain
        // clickable before and after the guest exits.
        self.app.want_window = true;
        self.app.sync_foreground();
        let _ = self
            .event_loop
            .pump_app_events(Some(timeout), &mut self.app);
        let (width, height) = self.app.desktop_size();
        for state in self.app.states() {
            let mut shared = state.borrow_mut();
            shared.gui.screen = (width, height);
            if shared.gui.dirty {
                shared.gui.dirty = false;
                self.app.needs_render = true;
            }
        }
        self.after_programs();
        let tray = self
            .app
            .tray_list()
            .into_iter()
            .map(|(_, _, item)| item)
            .collect();
        self.app.shell.set_tray(tray);
        if self.app.shell.clock_changed() {
            self.app.needs_render = true;
        }
        self.app.sync_title();
        self.app.run_script();
        self.app.render_if_requested();
    }

    /// Start programs the shell asked for and report the ones that ended.
    fn after_programs(&mut self) {
        let app = &mut self.app;
        while let Some(request) = app.shell.take_launch() {
            let Some(volume) = app.shell.volume().cloned() else {
                break;
            };
            let screen = app.desktop_size();
            match crate::shell::launch::spawn(&app.session, &volume, &request, screen) {
                Ok(pid) => eprintln!(
                    "[shell] started {} {} as process {pid}",
                    request.path, request.arguments
                ),
                Err(error) => app
                    .shell
                    .program_exited(Some(format!("{} could not start: {error}", request.name))),
            }
            app.needs_render = true;
        }
        for exit in app.session.take_exits() {
            let error = match exit.result {
                Ok(result) => {
                    eprintln!(
                        "[shell] {} (process {}) exited with status {:#x} after {} API calls",
                        exit.name, exit.pid, result.exit_code, result.api_calls
                    );
                    None
                }
                Err(error) => Some(format!("{} failed: {error}", exit.name)),
            };
            app.shell.program_exited(error);
            app.needs_render = true;
        }
        app.shell.running = app
            .session
            .programs()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        app.sync_foreground();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    fn right(self) -> i32 {
        self.x + self.width
    }

    fn bottom(self) -> i32 {
        self.y + self.height
    }

    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A taskbar entry. Guest entries are grouped from the guest's visible
/// top-level windows; the Explorer entry is the host-side shell window.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessEntry {
    name: String,
    title: String,
    hwnds: Vec<u32>,
    active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragMode {
    Move,
    Resize,
}

#[derive(Clone, Copy, Debug)]
struct DragState {
    hwnd: u32,
    mode: DragMode,
    grab_x: i32,
    grab_y: i32,
    start_x: i32,
    start_y: i32,
    start_width: i32,
    start_height: i32,
}

struct GuestApp {
    /// The foreground program's state (or `base` when none runs); input
    /// and window management act on it.
    shared: Rc<RefCell<SharedState>>,
    /// State of the program run directly (`--run --window`), or an idle
    /// placeholder on the desktop.
    base: Rc<RefCell<SharedState>>,
    /// Programs started from the desktop, running side by side.
    session: Rc<Session>,
    window: Option<Window>,
    gl_state: Option<GlState>,
    gl_surface: Option<Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    want_window: bool,
    needs_render: bool,
    initial_size_sent: bool,
    cursor: (i32, i32),
    drag: Option<DragState>,
    /// The desktop shell: icons, taskbar, Start menu, folder window.
    shell: Shell,
    modifiers: Mods,
    /// Scripted host input (`OTW_SHELL_INPUT`) and when its next command runs.
    script: Option<std::collections::VecDeque<String>>,
    script_resume: Option<std::time::Instant>,
    shell_only: bool,
    frame: Option<VideoFrame>,
    error: Option<DisplayError>,
}

impl GuestApp {
    fn new_with_shell(
        shared: Rc<RefCell<SharedState>>,
        shell: Shell,
        shell_only: bool,
        session: Rc<Session>,
    ) -> Self {
        Self {
            base: shared.clone(),
            shared,
            session,
            window: None,
            gl_state: None,
            gl_surface: None,
            gl_context: None,
            // The desktop itself is the window; shortcuts work even when no PE
            // guest is running yet.
            want_window: true,
            needs_render: true,
            initial_size_sent: false,
            cursor: (0, 0),
            drag: None,
            shell,
            modifiers: Mods::default(),
            script: None,
            script_resume: None,
            shell_only,
            frame: None,
            error: None,
        }
    }

    fn enqueue(&self, message: GuestMessage) {
        self.shared.borrow_mut().gui.enqueue(message);
    }

    fn main_hwnd(&self) -> u32 {
        self.shared.borrow().gui.main_hwnd
    }

    fn focus_hwnd(&self) -> u32 {
        self.shared.borrow().gui.focus_hwnd
    }

    fn activate(&mut self, hwnd: u32) {
        self.shell.focus_program();
        self.shared.borrow_mut().gui.activate_window(hwnd);
    }

    fn sync_title(&self) {
        let Some(window) = &self.window else {
            return;
        };
        let title = if self.shell_only && self.session.is_empty() {
            "Desktop".to_string()
        } else {
            self.shared
                .borrow()
                .gui
                .window(self.main_hwnd())
                .map(|window| window.title.clone())
                .unwrap_or_default()
        };
        window.set_title(&format!("OTW Desktop — {title}"));
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), DisplayError> {
        let attributes = WindowAttributes::default()
            .with_title("OTW Desktop")
            .with_inner_size(winit::dpi::LogicalSize {
                width: DESKTOP_WIDTH,
                height: DESKTOP_HEIGHT,
            })
            .with_resizable(true);
        let (window, config, display, not_current) = gl_context(event_loop, attributes)?;
        let (context, surface, gl) = make_current(&window, &config, &display, not_current)?;
        let placeholder = VideoFrame {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0],
            mode: crate::machine::VideoMode::Graphics320x200x256,
        };
        let state = GlState::new(gl, &placeholder)?;
        self.window = Some(window);
        self.gl_state = Some(state);
        self.gl_surface = Some(surface);
        self.gl_context = Some(context);
        self.clamp_windows();
        self.send_initial_size();
        self.needs_render = true;
        Ok(())
    }

    fn desktop_size(&self) -> (i32, i32) {
        self.window
            .as_ref()
            .map(|window| {
                let size = window.inner_size();
                (size.width as i32, size.height as i32)
            })
            .unwrap_or((DESKTOP_WIDTH as i32, DESKTOP_HEIGHT as i32))
    }

    fn clamp_windows(&self) {
        let (desktop_width, desktop_height) = self.desktop_size();
        let mut shared = self.shared.borrow_mut();
        for window in &mut shared.gui.windows {
            if window.parent != 0 || window.class.eq_ignore_ascii_case("edit") {
                continue;
            }
            let (x, y) = clamp_position(
                window.x,
                window.y,
                window.width,
                window.height,
                desktop_width,
                desktop_height,
            );
            window.x = x;
            window.y = y;
        }
        shared.gui.dirty = true;
    }

    fn send_initial_size(&mut self) {
        if self.initial_size_sent {
            return;
        }
        let sizes: Vec<(u32, i32, i32)> = {
            let shared = self.shared.borrow();
            if shared.gui.first_visible_top_level().is_none() {
                return;
            }
            shared
                .gui
                .windows
                .iter()
                .filter(|window| window.visible && window.parent == 0)
                .filter_map(|window| {
                    let client = client_rect(window);
                    (client.width > 0 && client.height > 0).then_some((
                        window.hwnd,
                        client.width,
                        client.height,
                    ))
                })
                .collect()
        };
        self.initial_size_sent = true;
        for (hwnd, width, height) in sizes {
            self.enqueue(size_message(hwnd, width, height));
        }
        self.needs_render = true;
    }

    fn hit_test(&self, x: i32, y: i32, title_only: bool) -> Option<u32> {
        hit_test_window(&self.shared.borrow().gui, x, y, title_only)
    }

    fn hit_test_close(&self, x: i32, y: i32) -> Option<u32> {
        hit_test_close_button(&self.shared.borrow().gui, x, y)
    }

    fn hit_test_resize(&self, x: i32, y: i32) -> Option<u32> {
        hit_test_resize_handle(&self.shared.borrow().gui, x, y)
    }

    /// The foreground program's state becomes the one input acts on.
    fn sync_foreground(&mut self) {
        let foreground = self
            .session
            .foreground()
            .map(|p| p.shared.clone())
            .unwrap_or_else(|| self.base.clone());
        if !Rc::ptr_eq(&foreground, &self.shared) {
            self.shared = foreground;
            self.needs_render = true;
        }
    }

    /// Every program's state, back to front.
    fn states(&self) -> Vec<Rc<RefCell<SharedState>>> {
        let programs = self.session.programs();
        if programs.is_empty() {
            vec![self.base.clone()]
        } else {
            programs.iter().map(|p| p.shared.clone()).collect()
        }
    }

    /// Taskbar buttons: each program's top-level windows, with the
    /// program they belong to (`None` for a directly run program).
    fn task_list(&self) -> Vec<(Task, Option<u32>, u32)> {
        let programs = self.session.programs();
        let foreground = programs.last().map(|p| p.pid);
        let mut tasks = Vec::new();
        let mut add = |shared: &SharedState, pid: Option<u32>, program: &str, front: bool| {
            for entry in process_entries(&shared.gui) {
                let hwnd = entry.hwnds.last().copied().unwrap_or(0);
                tasks.push((
                    Task {
                        label: entry.title,
                        active: entry.active && front,
                        program: program.to_string(),
                    },
                    pid,
                    hwnd,
                ));
            }
        };
        if programs.is_empty() {
            add(&self.base.borrow(), None, "", true);
        }
        for program in &programs {
            add(
                &program.shared.borrow(),
                Some(program.pid),
                &program.name,
                foreground == Some(program.pid),
            );
        }
        tasks
    }

    fn tasks(&self) -> Vec<Task> {
        self.task_list()
            .into_iter()
            .map(|(task, _, _)| task)
            .collect()
    }

    fn activate_task(&mut self, index: usize) {
        let Some((_, pid, hwnd)) = self.task_list().into_iter().nth(index) else {
            return;
        };
        if let Some(pid) = pid {
            self.session.bring_to_front(pid);
            self.sync_foreground();
        }
        if hwnd != 0 {
            let has_surface = self.shared.borrow().gdi.surfaces.contains_key(&hwnd);
            if has_surface {
                self.shared.borrow_mut().gui.pending_activate = Some(hwnd);
            } else {
                self.activate(hwnd);
            }
        }
        self.needs_render = true;
    }

    /// Every program's notification-area icons: (program state, index in
    /// that program's list, what the shell shows).
    fn tray_list(&self) -> Vec<(Rc<RefCell<SharedState>>, usize, TrayItem)> {
        let mut items = Vec::new();
        for state in self.states() {
            let shared = state.borrow();
            for (index, icon) in shared.gui.tray.iter().enumerate() {
                let image = shared.gui.small_icon(icon.icon).cloned();
                items.push((
                    state.clone(),
                    index,
                    TrayItem {
                        icon: image,
                        tip: icon.tip.clone(),
                    },
                ));
            }
        }
        items
    }

    /// A click on a notification-area icon goes to its program as the
    /// icon's callback message; pressing it brings the program forward, so
    /// a menu it opens gets the mouse.
    fn tray_event(&mut self, index: usize, event: TrayEvent) {
        let Some((state, local, _)) = self.tray_list().into_iter().nth(index) else {
            return;
        };
        if !matches!(event, TrayEvent::Up(_)) {
            if let Some(program) = self
                .session
                .programs()
                .into_iter()
                .find(|p| Rc::ptr_eq(&p.shared, &state))
            {
                self.session.bring_to_front(program.pid);
                self.sync_foreground();
            }
        }
        state.borrow_mut().gui.tray_event(local, event.message());
        self.needs_render = true;
    }

    /// Minimize a taskbar button's window, as clicking the button of the
    /// window in use does; the desktop takes the focus.
    fn minimize_task(&mut self, index: usize) {
        let Some((_, pid, hwnd)) = self.task_list().into_iter().nth(index) else {
            return;
        };
        let state = match pid {
            Some(pid) => self.session.program(pid).map(|p| p.shared.clone()),
            None => Some(self.base.clone()),
        };
        if let (Some(state), true) = (state, hwnd != 0) {
            state.borrow_mut().gui.enqueue(GuestMessage {
                hwnd,
                message: 0x0112,
                wparam: 0xF020,
                lparam: 0,
            }); // WM_SYSCOMMAND SC_MINIMIZE
        }
        self.shell.focus_desktop();
        self.needs_render = true;
    }

    /// Whether a program window (emulated or drawn by the fallback
    /// renderer) is under a desktop point.
    fn program_at(&self, x: i32, y: i32) -> bool {
        self.program_under(x, y).is_some()
            || (y < self.desktop_size().1 - TASKBAR_HEIGHT && self.hit_test(x, y, false).is_some())
    }

    /// The front-most program with a window under a desktop point.
    fn program_under(&self, x: i32, y: i32) -> Option<Rc<RefCell<SharedState>>> {
        let height = self.desktop_size().1;
        self.states()
            .into_iter()
            .rev()
            .find(|state| owns_point(&state.borrow(), x, y, height))
    }

    /// Whether host input at a desktop point belongs to the foreground
    /// program (a window with a surface, or a capture/modal).
    fn guest_owns_point(&self, x: i32, y: i32) -> bool {
        owns_point(&self.shared.borrow(), x, y, self.desktop_size().1)
    }

    fn guest_active(&self) -> bool {
        !self.shared.borrow().gdi.surfaces.is_empty()
    }

    fn push_input(&self, event: crate::win32::RawInput) {
        let mut shared = self.shared.borrow_mut();
        if let (crate::win32::RawInput::Move { .. }, Some(crate::win32::RawInput::Move { .. })) =
            (event, shared.gui.input.back())
        {
            shared.gui.input.pop_back();
        }
        shared.gui.input.push_back(event);
    }

    fn end_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let Some(window) = &self.window {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
        }
        let geometry = self
            .shared
            .borrow()
            .gui
            .window(drag.hwnd)
            .map(|window| (window.x, window.y, window.width, window.height));
        match (drag.mode, geometry) {
            (DragMode::Move, Some((x, y, _, _))) => {
                eprintln!("[gui] moved guest window {:#x} to ({x}, {y})", drag.hwnd);
            }
            (DragMode::Resize, Some((_, _, width, height))) => {
                eprintln!(
                    "[gui] resized guest window {:#x} to {width}x{height}",
                    drag.hwnd
                );
            }
            _ => {}
        }
    }

    /// Pointer motion from the host (or the input script).
    fn host_move(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
        if self.shell.pointer_move(x, y) {
            self.needs_render = true;
            return;
        }
        if self.drag.is_none() && self.guest_active() {
            self.push_input(crate::win32::RawInput::Move { x, y });
        }
        let Some(drag) = self.drag else {
            return;
        };
        let (desktop_width, desktop_height) = self.desktop_size();
        match drag.mode {
            DragMode::Move => {
                let moved = {
                    let mut shared = self.shared.borrow_mut();
                    if let Some(window) = shared.gui.window_mut(drag.hwnd) {
                        let (next_x, next_y) = clamp_position(
                            x - drag.grab_x,
                            y - drag.grab_y,
                            window.width,
                            window.height,
                            desktop_width,
                            desktop_height,
                        );
                        if next_x == window.x && next_y == window.y {
                            None
                        } else {
                            window.x = next_x;
                            window.y = next_y;
                            Some((next_x, next_y))
                        }
                    } else {
                        None
                    }
                };
                if let Some((x, y)) = moved {
                    self.shared.borrow_mut().gui.dirty = true;
                    self.enqueue(move_message(drag.hwnd, x, y));
                    self.needs_render = true;
                }
            }
            DragMode::Resize => {
                let usable_height = (desktop_height - TASKBAR_HEIGHT).max(MIN_WINDOW_HEIGHT);
                let resized = {
                    let mut shared = self.shared.borrow_mut();
                    if let Some(window) = shared.gui.window_mut(drag.hwnd) {
                        let max_width = (desktop_width - window.x).max(MIN_WINDOW_WIDTH);
                        let max_height = (usable_height - window.y).max(MIN_WINDOW_HEIGHT);
                        let width = (drag.start_width + x - drag.start_x)
                            .clamp(MIN_WINDOW_WIDTH, max_width);
                        let height = (drag.start_height + y - drag.start_y)
                            .clamp(MIN_WINDOW_HEIGHT, max_height);
                        if width == window.width && height == window.height {
                            None
                        } else {
                            window.width = width;
                            window.height = height;
                            let client = client_rect(window);
                            Some((client.width, client.height))
                        }
                    } else {
                        None
                    }
                };
                if let Some((width, height)) = resized {
                    self.shared.borrow_mut().gui.dirty = true;
                    self.enqueue(size_message(drag.hwnd, width, height));
                    self.needs_render = true;
                }
            }
        }
    }

    /// A mouse button from the host (or the input script).
    fn host_button(&mut self, button: MouseButton, down: bool) {
        let state = if down {
            ElementState::Pressed
        } else {
            ElementState::Released
        };
        let (x, y) = self.cursor;
        let down = state == ElementState::Pressed;
        // The shell sees every click first: its menus, dialogs,
        // taskbar, folder window and desktop.
        let shell_button = match button {
            MouseButton::Left => Some(ShellButton::Left),
            MouseButton::Right => Some(ShellButton::Right),
            _ => None,
        };
        if let Some(shell_button) = shell_button {
            let program_hit = self.program_at(x, y);
            let program_focused = !self.shell.wants_keyboard();
            let outcome = if down {
                let tasks = self.tasks();
                self.shell
                    .pointer_down(x, y, shell_button, program_hit, &tasks)
            } else {
                self.shell.pointer_up(x, y, shell_button, program_hit)
            };
            match outcome {
                PointerOutcome::Handled => {
                    // The desktop or folder window took the focus.
                    if down && self.shell.wants_keyboard() {
                        self.session.deactivate_foreground();
                    }
                    self.needs_render = true;
                    return;
                }
                PointerOutcome::Task(index) => {
                    // The button of the window in use minimizes it.
                    let active = self.tasks().get(index).is_some_and(|task| task.active);
                    if program_focused && active {
                        self.minimize_task(index);
                    } else {
                        self.activate_task(index);
                    }
                    return;
                }
                PointerOutcome::Tray(index, event) => {
                    self.tray_event(index, event);
                    return;
                }
                PointerOutcome::Unhandled => {
                    // A click on a background program brings it forward.
                    if down {
                        if let Some(state) = self.program_under(x, y) {
                            if let Some(program) = self
                                .session
                                .programs()
                                .into_iter()
                                .find(|p| Rc::ptr_eq(&p.shared, &state))
                            {
                                self.session.bring_to_front(program.pid);
                                self.sync_foreground();
                            }
                        }
                    }
                }
            }
        }
        if button != MouseButton::Left {
            let index = if button == MouseButton::Right { 1 } else { 2 };
            if (!down || self.guest_owns_point(x, y)) && self.guest_active() {
                self.push_input(crate::win32::RawInput::Button {
                    x,
                    y,
                    button: index,
                    down,
                });
            }
            return;
        }
        match state {
            ElementState::Pressed => {
                if self.guest_owns_point(x, y) {
                    self.push_input(crate::win32::RawInput::Button {
                        x,
                        y,
                        button: 0,
                        down: true,
                    });
                    self.needs_render = true;
                    return;
                }
                if let Some(hwnd) = self.hit_test_close(x, y) {
                    self.enqueue(GuestMessage {
                        hwnd,
                        message: 0x0010, // WM_CLOSE
                        wparam: 0,
                        lparam: 0,
                    });
                    self.needs_render = true;
                    return;
                }
                if let Some(hwnd) = self.hit_test_resize(x, y) {
                    self.activate(hwnd);
                    let geometry = {
                        let shared = self.shared.borrow();
                        shared
                            .gui
                            .window(hwnd)
                            .map(|window| (window.x, window.y, window.width, window.height))
                    };
                    if let Some((_, _, width, height)) = geometry {
                        self.drag = Some(DragState {
                            hwnd,
                            mode: DragMode::Resize,
                            grab_x: 0,
                            grab_y: 0,
                            start_x: x,
                            start_y: y,
                            start_width: width,
                            start_height: height,
                        });
                        if let Some(window) = &self.window {
                            let _ = window.set_cursor_grab(CursorGrabMode::Confined);
                        }
                    }
                    self.needs_render = true;
                    return;
                }
                if let Some(hwnd) = self.hit_test(x, y, true) {
                    self.activate(hwnd);
                    let grab = {
                        let shared = self.shared.borrow();
                        shared
                            .gui
                            .window(hwnd)
                            .map(|window| (x - window.x, y - window.y))
                    };
                    if let Some((grab_x, grab_y)) = grab {
                        self.drag = Some(DragState {
                            hwnd,
                            mode: DragMode::Move,
                            grab_x,
                            grab_y,
                            start_x: x,
                            start_y: y,
                            start_width: 0,
                            start_height: 0,
                        });
                        if let Some(window) = &self.window {
                            let _ = window.set_cursor_grab(CursorGrabMode::Confined);
                        }
                    }
                    self.needs_render = true;
                    return;
                }
                if let Some(hwnd) = self.hit_test(x, y, false) {
                    self.activate(hwnd);
                    self.needs_render = true;
                }
            }
            ElementState::Released => {
                if self.drag.is_none() && self.guest_active() {
                    self.push_input(crate::win32::RawInput::Button {
                        x,
                        y,
                        button: 0,
                        down: false,
                    });
                }
                self.end_drag()
            }
        }
    }

    /// A key from the host (or the input script): the shell's view of it,
    /// the Win32 virtual key, and the character it types.
    fn host_key(&mut self, key: Option<ShellKey>, vk: Option<u32>, text: Option<u32>, down: bool) {
        // Shell shortcuts (Ctrl+Esc, the logo key) always work; the rest go
        // to the shell while it has the focus. Key releases always reach the
        // program so it never sees a stuck key.
        if down {
            if let Some(key) = key {
                if self.shell.key(key, self.modifiers) {
                    self.needs_render = true;
                    return;
                }
            }
            if self.shell.wants_keyboard() {
                return;
            }
        }
        if self.guest_active() {
            if let Some(vk) = vk {
                let ch = if down {
                    text.filter(|ch| *ch < 0x100).or(match vk {
                        0x08 => Some(0x08),
                        0x09 => Some(0x09),
                        0x0D => Some(0x0D),
                        0x1B => Some(0x1B),
                        _ => None,
                    })
                } else {
                    None
                };
                // Ctrl+letter produces the control character, as on Win32.
                let ctrl = self.shared.borrow().gui.keys[0x11] & 0x80 != 0;
                let ch = if ctrl && (0x41..=0x5A).contains(&vk) && down {
                    Some(vk - 0x40)
                } else {
                    ch
                };
                self.push_input(crate::win32::RawInput::Key { vk, down, ch });
            }
            return;
        }
        let character = text.or(match vk {
            Some(0x08) => Some(0x08),
            Some(0x0D) => Some(0x0D),
            Some(0x09) => Some(0x09),
            _ => None,
        });
        if let (Some(character), true) = (character, down) {
            let hwnd = self.focus_hwnd();
            if hwnd != 0 {
                self.enqueue(GuestMessage {
                    hwnd,
                    message: 0x0102, // WM_CHAR
                    wparam: character,
                    lparam: 1,
                });
            }
        }
    }

    /// Run the next `OTW_SHELL_INPUT` command. The script drives the desktop
    /// exactly like host input, e.g. `w 5; m 20 680; d; u; k p; t hello;
    /// kd ctrl; k s; ku ctrl; s /tmp/shot.ppm`:
    /// `m x y` move, `d`/`u` and `rd`/`ru` left and right button, `k key`
    /// press and release (a character or Enter, Esc, Tab, Back, Del, Up,
    /// Down, Left, Right, Home, End, F2, F4, F5, F10, Apps, Win), `kd`/`ku`
    /// hold or release ctrl, alt, shift, `t text` type, `w n` wait n tenths
    /// of a second, `s path` save a PPM screenshot, `q` shut down.
    fn run_script(&mut self) {
        let script = self.script.get_or_insert_with(|| {
            std::env::var("OTW_SHELL_INPUT")
                .unwrap_or_default()
                .split(';')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect()
        });
        if self
            .script_resume
            .is_some_and(|t| std::time::Instant::now() < t)
        {
            return;
        }
        self.script_resume = None;
        let Some(command) = script.pop_front() else {
            return;
        };
        let (verb, rest) = command.split_once(' ').unwrap_or((command.as_str(), ""));
        let number = |index: usize| {
            rest.split_whitespace()
                .nth(index)
                .and_then(|n| n.parse::<i32>().ok())
                .unwrap_or(0)
        };
        match verb {
            "m" => self.host_move(number(0), number(1)),
            "d" | "u" => self.host_button(MouseButton::Left, verb == "d"),
            "rd" | "ru" => self.host_button(MouseButton::Right, verb == "rd"),
            "k" => {
                if let Some((key, vk, ch)) = script_key(rest) {
                    self.host_key(Some(key), Some(vk), ch, true);
                    self.host_key(None, Some(vk), None, false);
                }
            }
            "kd" | "ku" => {
                let down = verb == "kd";
                let vk = match rest {
                    "ctrl" => {
                        self.modifiers.ctrl = down;
                        0x11
                    }
                    "alt" => {
                        self.modifiers.alt = down;
                        0x12
                    }
                    "shift" => {
                        self.modifiers.shift = down;
                        0x10
                    }
                    _ => return,
                };
                self.host_key(None, Some(vk), None, down);
            }
            "t" => {
                for ch in rest.chars() {
                    if let Some((key, vk, text)) = script_key(&ch.to_string()) {
                        self.host_key(Some(key), Some(vk), text, true);
                        self.host_key(None, Some(vk), None, false);
                    }
                }
            }
            "w" => {
                self.script_resume = Some(
                    std::time::Instant::now()
                        + Duration::from_millis(100 * number(0).max(0) as u64),
                )
            }
            "s" => {
                let (width, height) = self.desktop_size();
                self.shell.set_screen(width, height);
                let tasks = self.tasks();
                let pixels = self.composite(width as usize, height as usize, &tasks);
                let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
                ppm.extend_from_slice(&pixels);
                if let Err(error) = std::fs::write(rest, ppm) {
                    eprintln!("[script] screenshot {rest}: {error}");
                }
            }
            "q" => self.shell.quit = true,
            _ => eprintln!("[script] unknown command {command:?}"),
        }
        self.needs_render = true;
    }

    fn composite(&self, width: usize, height: usize, tasks: &[Task]) -> Vec<u8> {
        let states = self.states();
        let borrowed: Vec<std::cell::Ref<'_, SharedState>> =
            states.iter().map(|state| state.borrow()).collect();
        let programs: Vec<&SharedState> = borrowed.iter().map(|state| &**state).collect();
        render_desktop(width, height, &programs, &self.shell, tasks)
    }

    fn render_if_requested(&mut self) {
        if !self.needs_render || self.gl_state.is_none() {
            return;
        }
        self.needs_render = false;
        let (width, height) = self
            .window
            .as_ref()
            .map(|window| {
                let size = window.inner_size();
                (size.width.max(1), size.height.max(1))
            })
            .unwrap_or((640, 440));
        self.shell.set_screen(width as i32, height as i32);
        let tasks = self.tasks();
        let pixels = self.composite(width as usize, height as usize, &tasks);
        let frame = VideoFrame {
            width: width as usize,
            height: height as usize,
            pixels,
            mode: crate::machine::VideoMode::Graphics320x200x256,
        };
        if let Some(state) = &self.gl_state {
            state.update(&frame);
            state.draw(width, height);
        }
        if let (Some(context), Some(surface)) = (&self.gl_context, &self.gl_surface) {
            let _ = surface.swap_buffers(context);
        }
        self.frame = Some(frame);
        self.shared.borrow_mut().gui.dirty = false;
    }
}

impl ApplicationHandler for GuestApp {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: winit::event::StartCause) {
        if self.want_window && self.window.is_none() && self.error.is_none() {
            if let Err(error) = self.create_window(event_loop) {
                self.error = Some(error);
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.want_window && self.window.is_none() && self.error.is_none() {
            if let Err(error) = self.create_window(event_loop) {
                self.error = Some(error);
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                if self.shell_only && self.session.is_empty() {
                    self.window = None;
                    event_loop.exit();
                    return;
                }
                // Let the guest's own WndProc handle WM_CLOSE and call
                // PostQuitMessage, which ends its message loop.
                let hwnd = self.main_hwnd();
                if hwnd != 0 {
                    self.enqueue(GuestMessage {
                        hwnd,
                        message: 0x0010, // WM_CLOSE
                        wparam: 0,
                        lparam: 0,
                    });
                } else if self.shell_only {
                    self.shared.borrow_mut().gui.quit = true;
                }
                if !self.shell_only {
                    if let Some(window) = &self.window {
                        window.set_visible(false);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let (Some(context), Some(surface)) = (&self.gl_context, &self.gl_surface) {
                    if let (Some(width), Some(height)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        let _ = surface.resize(context, width, height);
                    }
                }
                // The host window is the desktop; resizing it must not resize
                // the guest window. Keep guest windows inside the new area.
                self.clamp_windows();
                self.needs_render = true;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.host_move(position.x as i32, position.y as i32)
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                self.modifiers = Mods {
                    ctrl: state.control_key(),
                    alt: state.alt_key(),
                    shift: state.shift_key(),
                };
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = self.cursor;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, dy) => dy.round() as i32,
                    MouseScrollDelta::PixelDelta(position) => (position.y / 20.0).round() as i32,
                };
                if lines != 0 && self.shell.wheel(x, y, lines) {
                    self.needs_render = true;
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.host_button(button, state == ElementState::Pressed)
            }
            WindowEvent::KeyboardInput {
                is_synthetic: true, ..
            } => {}
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                let vk = virtual_key(&event.logical_key, event.physical_key);
                let text = event.text.as_ref().and_then(|text| text.chars().next()).or(
                    match &event.logical_key {
                        Key::Character(text) => text.chars().next(),
                        _ => None,
                    },
                );
                self.host_key(shell_key(&event), vk, text.map(|ch| ch as u32), down);
            }
            WindowEvent::RedrawRequested => self.needs_render = true,
            _ => {}
        }
    }
}

/// A key named in an input script: the shell key, Win32 virtual key and
/// typed character.
fn script_key(name: &str) -> Option<(ShellKey, u32, Option<u32>)> {
    Some(match name {
        "Enter" => (ShellKey::Enter, 0x0D, Some(0x0D)),
        "Esc" => (ShellKey::Escape, 0x1B, Some(0x1B)),
        "Tab" => (ShellKey::Tab, 0x09, Some(0x09)),
        "Back" => (ShellKey::Backspace, 0x08, Some(0x08)),
        "Del" => (ShellKey::Delete, 0x2E, None),
        "Up" => (ShellKey::Up, 0x26, None),
        "Down" => (ShellKey::Down, 0x28, None),
        "Left" => (ShellKey::Left, 0x25, None),
        "Right" => (ShellKey::Right, 0x27, None),
        "Home" => (ShellKey::Home, 0x24, None),
        "End" => (ShellKey::End, 0x23, None),
        "F2" => (ShellKey::F2, 0x71, None),
        "F4" => (ShellKey::F4, 0x73, None),
        "F5" => (ShellKey::F5, 0x74, None),
        "F10" => (ShellKey::F10, 0x79, None),
        "Apps" => (ShellKey::Apps, 0x5D, None),
        "Win" => (ShellKey::Logo, 0x5B, None),
        "Space" => (ShellKey::Char(' '), 0x20, Some(0x20)),
        text => {
            let ch = text.chars().next()?;
            let vk = match ch.to_ascii_uppercase() {
                c @ ('A'..='Z' | '0'..='9') => c as u32,
                ' ' => 0x20,
                '.' => 0xBE,
                ',' => 0xBC,
                '-' => 0xBD,
                '\\' => 0xDC,
                '/' => 0xBF,
                ':' => 0xBA,
                _ => 0xFF,
            };
            (ShellKey::Char(ch), vk, Some(ch as u32))
        }
    })
}

/// Translate a host key press into a shell key.
fn shell_key(event: &KeyEvent) -> Option<ShellKey> {
    Some(match &event.logical_key {
        Key::Named(named) => match named {
            NamedKey::ArrowUp => ShellKey::Up,
            NamedKey::ArrowDown => ShellKey::Down,
            NamedKey::ArrowLeft => ShellKey::Left,
            NamedKey::ArrowRight => ShellKey::Right,
            NamedKey::Home => ShellKey::Home,
            NamedKey::End => ShellKey::End,
            NamedKey::PageUp => ShellKey::PageUp,
            NamedKey::PageDown => ShellKey::PageDown,
            NamedKey::Enter => ShellKey::Enter,
            NamedKey::Escape => ShellKey::Escape,
            NamedKey::Backspace => ShellKey::Backspace,
            NamedKey::Delete => ShellKey::Delete,
            NamedKey::Tab => ShellKey::Tab,
            NamedKey::F2 => ShellKey::F2,
            NamedKey::F4 => ShellKey::F4,
            NamedKey::F5 => ShellKey::F5,
            NamedKey::F10 => ShellKey::F10,
            NamedKey::ContextMenu => ShellKey::Apps,
            NamedKey::Super | NamedKey::Meta => ShellKey::Logo,
            NamedKey::Space => ShellKey::Char(' '),
            _ => return None,
        },
        Key::Character(text) => ShellKey::Char(text.chars().next()?),
        _ => return None,
    })
}

/// Translate a host key into a Win32 virtual-key code.
fn virtual_key(key: &Key, physical: winit::keyboard::PhysicalKey) -> Option<u32> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    if let PhysicalKey::Code(code) = physical {
        let vk = match code {
            KeyCode::ShiftLeft | KeyCode::ShiftRight => Some(0x10),
            KeyCode::ControlLeft | KeyCode::ControlRight => Some(0x11),
            KeyCode::AltLeft | KeyCode::AltRight => Some(0x12),
            KeyCode::Digit0 => Some(0x30),
            KeyCode::Digit1 => Some(0x31),
            KeyCode::Digit2 => Some(0x32),
            KeyCode::Digit3 => Some(0x33),
            KeyCode::Digit4 => Some(0x34),
            KeyCode::Digit5 => Some(0x35),
            KeyCode::Digit6 => Some(0x36),
            KeyCode::Digit7 => Some(0x37),
            KeyCode::Digit8 => Some(0x38),
            KeyCode::Digit9 => Some(0x39),
            _ => None,
        };
        if vk.is_some() {
            return vk;
        }
    }
    Some(match key {
        Key::Named(named) => match named {
            NamedKey::Enter => 0x0D,
            NamedKey::Tab => 0x09,
            NamedKey::Backspace => 0x08,
            NamedKey::Escape => 0x1B,
            NamedKey::Space => 0x20,
            NamedKey::ArrowLeft => 0x25,
            NamedKey::ArrowUp => 0x26,
            NamedKey::ArrowRight => 0x27,
            NamedKey::ArrowDown => 0x28,
            NamedKey::Home => 0x24,
            NamedKey::End => 0x23,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::Insert => 0x2D,
            NamedKey::Delete => 0x2E,
            NamedKey::Shift => 0x10,
            NamedKey::Control => 0x11,
            NamedKey::Alt => 0x12,
            NamedKey::CapsLock => 0x14,
            NamedKey::F1 => 0x70,
            NamedKey::F2 => 0x71,
            NamedKey::F3 => 0x72,
            NamedKey::F4 => 0x73,
            NamedKey::F5 => 0x74,
            NamedKey::F6 => 0x75,
            NamedKey::F7 => 0x76,
            NamedKey::F8 => 0x77,
            NamedKey::F9 => 0x78,
            NamedKey::F10 => 0x79,
            NamedKey::F11 => 0x7A,
            NamedKey::F12 => 0x7B,
            _ => return None,
        },
        Key::Character(text) => {
            let ch = text.chars().next()?;
            match ch.to_ascii_uppercase() {
                c @ 'A'..='Z' => c as u32,
                c @ '0'..='9' => c as u32,
                ' ' => 0x20,
                ';' | ':' => 0xBA,
                '=' | '+' => 0xBB,
                ',' | '<' => 0xBC,
                '-' | '_' => 0xBD,
                '.' | '>' => 0xBE,
                '/' | '?' => 0xBF,
                '`' | '~' => 0xC0,
                '[' | '{' => 0xDB,
                '\\' | '|' => 0xDC,
                ']' | '}' => 0xDD,
                '\'' | '"' => 0xDE,
                _ => return None,
            }
        }
        _ => return None,
    })
}

// ── Software renderer ──────────────────────────────────────────────────────

const CHAR_W: usize = 10;
const CHAR_H: usize = 14;
const FONT_SCALE: usize = 2;

const FACE_COLOR: (u8, u8, u8) = (0xc0, 0xc0, 0xc0);
const HIGHLIGHT_COLOR: (u8, u8, u8) = (0xff, 0xff, 0xff);
const SHADOW_COLOR: (u8, u8, u8) = (0x40, 0x40, 0x40);
const MENU_COLOR: (u8, u8, u8) = (0xd4, 0xd0, 0xc8);
const ACTIVE_TITLE: (u8, u8, u8) = (0x00, 0x00, 0x80);
const INACTIVE_TITLE: (u8, u8, u8) = (0x80, 0x80, 0x80);

fn window_rect(window: &crate::win32::GuestWindow) -> Rect {
    Rect {
        x: window.x,
        y: window.y,
        width: window.width,
        height: window.height,
    }
}

fn clamp_position(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    desktop_width: i32,
    desktop_height: i32,
) -> (i32, i32) {
    let usable_height = (desktop_height - TASKBAR_HEIGHT).max(TITLE_HEIGHT + WINDOW_BORDER * 2);
    (
        x.clamp(0, (desktop_width - width).max(0)),
        y.clamp(0, (usable_height - height).max(0)),
    )
}

/// Whether input at a desktop point belongs to a program: it has the mouse
/// captured or owns the top-level window there.
fn owns_point(shared: &SharedState, x: i32, y: i32, desktop_height: i32) -> bool {
    if shared.gui.capture != 0 {
        return true;
    }
    if y >= desktop_height - TASKBAR_HEIGHT {
        return false;
    }
    crate::win32::user::top_level_at(&shared.gui, x, y)
        .map(|hwnd| shared.gdi.surfaces.contains_key(&hwnd))
        .unwrap_or(false)
}

fn process_entries(gui: &GuiState) -> Vec<ProcessEntry> {
    let mut entries: Vec<ProcessEntry> = Vec::new();
    for window in &gui.windows {
        if !window.visible || window.parent != 0 || window.class.eq_ignore_ascii_case("edit") {
            continue;
        }
        if window.owner != 0 || window.class == "#32768" || window.class == "ComboLBox" {
            continue;
        }
        let name = if window.class.trim().is_empty() {
            "Program".to_string()
        } else {
            window.class.clone()
        };
        if let Some(entry) = entries.iter_mut().find(|entry| entry.name == name) {
            entry.hwnds.push(window.hwnd);
            entry.active |= window.hwnd == gui.active_hwnd;
        } else {
            let title = if window.title.trim().is_empty() {
                name.clone()
            } else {
                window.title.clone()
            };
            entries.push(ProcessEntry {
                name,
                title,
                hwnds: vec![window.hwnd],
                active: window.hwnd == gui.active_hwnd,
            });
        }
    }
    entries
}

fn hit_test_window(gui: &GuiState, x: i32, y: i32, title_only: bool) -> Option<u32> {
    for window in gui.windows.iter().rev() {
        if !window.visible || window.parent != 0 || window.class.eq_ignore_ascii_case("edit") {
            continue;
        }
        let rect = window_rect(window);
        if !rect.contains(x, y) {
            continue;
        }
        if title_only && y >= rect.y + WINDOW_BORDER + TITLE_HEIGHT {
            continue;
        }
        return Some(window.hwnd);
    }
    None
}

fn hit_test_close_button(gui: &GuiState, x: i32, y: i32) -> Option<u32> {
    gui.windows
        .iter()
        .rev()
        .find(|window| {
            window.visible
                && window.parent == 0
                && !window.class.eq_ignore_ascii_case("edit")
                && close_button_rect(window).contains(x, y)
        })
        .map(|window| window.hwnd)
}

fn move_message(hwnd: u32, x: i32, y: i32) -> GuestMessage {
    GuestMessage {
        hwnd,
        message: 0x0003, // WM_MOVE
        wparam: 0,
        lparam: ((y as u16 as u32) << 16) | (x as u16 as u32),
    }
}

fn size_message(hwnd: u32, width: i32, height: i32) -> GuestMessage {
    GuestMessage {
        hwnd,
        message: 0x0005, // WM_SIZE
        wparam: 0,       // SIZE_RESTORED
        lparam: ((height as u16 as u32) << 16) | (width as u16 as u32),
    }
}

fn hit_test_resize_handle(gui: &GuiState, x: i32, y: i32) -> Option<u32> {
    gui.windows
        .iter()
        .rev()
        .find(|window| {
            window.visible
                && window.parent == 0
                && !window.class.eq_ignore_ascii_case("edit")
                && resize_handle_rect(window).contains(x, y)
        })
        .map(|window| window.hwnd)
}

fn close_button_rect(window: &crate::win32::GuestWindow) -> Rect {
    let outer = window_rect(window);
    Rect {
        x: outer.right() - WINDOW_BORDER - CLOSE_BUTTON - 2,
        y: outer.y + WINDOW_BORDER + 3,
        width: CLOSE_BUTTON,
        height: CLOSE_BUTTON,
    }
}

fn resize_handle_rect(window: &crate::win32::GuestWindow) -> Rect {
    let outer = window_rect(window);
    Rect {
        x: outer.right() - WINDOW_BORDER - RESIZE_HANDLE,
        y: outer.bottom() - WINDOW_BORDER - RESIZE_HANDLE,
        width: RESIZE_HANDLE,
        height: RESIZE_HANDLE,
    }
}

fn client_rect(window: &crate::win32::GuestWindow) -> Rect {
    let outer = window_rect(window);
    Rect {
        x: outer.x + WINDOW_BORDER,
        y: outer.y + WINDOW_BORDER + TITLE_HEIGHT + MENU_HEIGHT,
        width: outer.width - WINDOW_BORDER * 2,
        height: outer.height - WINDOW_BORDER * 2 - TITLE_HEIGHT - MENU_HEIGHT,
    }
}

/// Composite the desktop: `programs` are drawn back to front, each with
/// its own windows and surfaces.
fn render_desktop(
    width: usize,
    height: usize,
    programs: &[&SharedState],
    shell: &Shell,
    tasks: &[Task],
) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    // The shell paints the desktop (and a background folder window), the
    // program windows go on top, then the foreground folder window, the
    // taskbar, menus and dialogs.
    let mut screen = crate::win32::gfx::Surface::new(width as i32, height as i32, 24);
    shell.paint(&mut screen, Layer::Background, tasks);
    let mut pixels = surface_pixels(&screen);

    // Each program's top-level windows in its own z-order; the foreground
    // program comes last.
    for program in programs {
        let (gui, surfaces) = (&program.gui, &program.gdi.surfaces);
        for window in &gui.windows {
            if !window.visible
                || window.minimized
                || window.style & crate::win32::user::WS_CHILD != 0
            {
                continue;
            }
            if let Some(surface) = surfaces.get(&window.hwnd) {
                blit_surface(&mut pixels, width, height, surface, window.x, window.y);
                continue;
            }
            if window.parent != 0 || window.class.eq_ignore_ascii_case("edit") {
                continue;
            }
            if window.width <= WINDOW_BORDER * 2
                || window.height <= WINDOW_BORDER * 2 + TITLE_HEIGHT + MENU_HEIGHT
            {
                continue;
            }
            draw_window(
                &mut pixels,
                width,
                height,
                window,
                window.hwnd == gui.active_hwnd,
                gui,
            );
        }
    }
    let mut screen = surface_from_pixels(&pixels, width, height);
    shell.paint(&mut screen, Layer::Foreground, tasks);
    shell.paint(&mut screen, Layer::Top, tasks);
    surface_pixels(&screen)
}

fn surface_pixels(surface: &crate::win32::gfx::Surface) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(surface.pixels.len() * 3);
    for pixel in &surface.pixels {
        pixels.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
    }
    pixels
}

fn surface_from_pixels(pixels: &[u8], width: usize, height: usize) -> crate::win32::gfx::Surface {
    let mut surface = crate::win32::gfx::Surface::new(width as i32, height as i32, 24);
    for (target, rgb) in surface.pixels.iter_mut().zip(pixels.chunks_exact(3)) {
        *target = (rgb[0] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[2] as u32;
    }
    surface
}

/// Copy a guest window's backing surface onto the desktop frame.
fn blit_surface(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    surface: &crate::win32::gfx::Surface,
    x: i32,
    y: i32,
) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + surface.width).min(width as i32);
    let y1 = (y + surface.height).min(height as i32);
    for dy in y0..y1 {
        let row = ((dy - y) * surface.width) as usize;
        let out = (dy as usize * width) * 3;
        for dx in x0..x1 {
            let pixel = surface.pixels[row + (dx - x) as usize];
            let index = out + dx as usize * 3;
            pixels[index] = (pixel >> 16) as u8;
            pixels[index + 1] = (pixel >> 8) as u8;
            pixels[index + 2] = pixel as u8;
        }
    }
}

fn draw_window(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    window: &crate::win32::GuestWindow,
    active: bool,
    gui: &crate::win32::GuiState,
) {
    let outer = window_rect(window);
    fill_rect(pixels, width, height, outer, (0, 0, 0));
    fill_rect(pixels, width, height, inset(outer, 1), FACE_COLOR);
    fill_rect(pixels, width, height, inset(outer, 2), SHADOW_COLOR);

    let title = Rect {
        x: outer.x + WINDOW_BORDER,
        y: outer.y + WINDOW_BORDER,
        width: outer.width - WINDOW_BORDER * 2,
        height: TITLE_HEIGHT,
    };
    fill_rect(
        pixels,
        width,
        height,
        title,
        if active { ACTIVE_TITLE } else { INACTIVE_TITLE },
    );
    draw_text_in(
        pixels,
        width,
        height,
        title,
        title.x + 4,
        title.y + 3,
        window.title.as_bytes(),
        HIGHLIGHT_COLOR,
    );

    let close = close_button_rect(window);
    fill_rect(pixels, width, height, close, (0, 0, 0));
    fill_rect(pixels, width, height, inset(close, 1), MENU_COLOR);
    draw_text_in(
        pixels,
        width,
        height,
        inset(close, 1),
        close.x + 4,
        close.y + 2,
        b"x",
        (0, 0, 0),
    );

    let menu = Rect {
        x: outer.x + WINDOW_BORDER,
        y: outer.y + WINDOW_BORDER + TITLE_HEIGHT,
        width: outer.width - WINDOW_BORDER * 2,
        height: MENU_HEIGHT,
    };
    fill_rect(pixels, width, height, menu, MENU_COLOR);
    draw_text_in(
        pixels,
        width,
        height,
        menu,
        menu.x + 6,
        menu.y + 5,
        window.class.as_bytes(),
        (0, 0, 0),
    );

    let client = client_rect(window);
    fill_rect(pixels, width, height, client, HIGHLIGHT_COLOR);

    let text = gui.client_text_for(window.hwnd).unwrap_or(&[]);
    let mut line = 0i32;
    let mut column = 0i32;
    for &byte in text {
        match byte {
            b'\n' => {
                line += 1;
                column = 0;
                continue;
            }
            b'\r' => continue,
            0x20..=0x7e => {}
            _ => {
                column += 1;
                continue;
            }
        }
        let x = client.x + 3 + column * CHAR_W as i32;
        let y = client.y + 3 + line * (CHAR_H as i32 + 4);
        draw_text_in(pixels, width, height, client, x, y, &[byte], (0, 0, 0));
        column += 1;
    }

    if gui.top_level_for(gui.focus_hwnd) == Some(window.hwnd) {
        let caret = Rect {
            x: client.x + 3 + column * CHAR_W as i32,
            y: client.y + 3 + line * (CHAR_H as i32 + 4),
            width: 1,
            height: CHAR_H as i32,
        };
        fill_rect(pixels, width, height, caret, (0, 0, 0));
    }

    let grip = resize_handle_rect(window);
    fill_rect(pixels, width, height, grip, FACE_COLOR);
    for index in 0..3 {
        fill_rect(
            pixels,
            width,
            height,
            Rect {
                x: grip.x + 3 + index * 4,
                y: grip.bottom() - 5 - index * 4,
                width: 2,
                height: 2,
            },
            SHADOW_COLOR,
        );
    }
}

fn inset(rect: Rect, amount: i32) -> Rect {
    Rect {
        x: rect.x + amount,
        y: rect.y + amount,
        width: rect.width - amount * 2,
        height: rect.height - amount * 2,
    }
}

fn fill_rect(
    pixels: &mut [u8],
    frame_width: usize,
    frame_height: usize,
    rect: Rect,
    color: (u8, u8, u8),
) {
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = rect.right().min(frame_width as i32);
    let y1 = rect.bottom().min(frame_height as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            let offset = (y as usize * frame_width + x as usize) * 3;
            pixels[offset] = color.0;
            pixels[offset + 1] = color.1;
            pixels[offset + 2] = color.2;
        }
    }
}

fn draw_text_in(
    pixels: &mut [u8],
    frame_width: usize,
    frame_height: usize,
    clip: Rect,
    origin_x: i32,
    origin_y: i32,
    text: &[u8],
    color: (u8, u8, u8),
) {
    for (index, &byte) in text.iter().enumerate() {
        if byte == b' ' {
            continue;
        }
        let glyph_index = if (b' '..=b'~').contains(&byte) {
            (byte - b' ') as usize
        } else {
            0
        };
        let start = glyph_index * 35;
        let Some(glyph) = FONT_DATA.get(start..start + 35) else {
            continue;
        };
        let char_x = origin_x + index as i32 * CHAR_W as i32;
        for row in 0..7 {
            for column in 0..5 {
                if glyph[row * 5 + column] != b'#' {
                    continue;
                }
                for sy in 0..FONT_SCALE {
                    for sx in 0..FONT_SCALE {
                        let x = char_x + (column * FONT_SCALE + sx) as i32;
                        let y = origin_y + (row * FONT_SCALE + sy) as i32;
                        if x < clip.x || y < clip.y || x >= clip.right() || y >= clip.bottom() {
                            continue;
                        }
                        if x < 0 || y < 0 || x as usize >= frame_width || y as usize >= frame_height
                        {
                            continue;
                        }
                        let offset = (y as usize * frame_width + x as usize) * 3;
                        pixels[offset] = color.0;
                        pixels[offset + 1] = color.1;
                        pixels[offset + 2] = color.2;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "display"))]
mod tests {
    use super::*;
    use crate::win32::GuestWindow;

    fn sample_window() -> GuestWindow {
        GuestWindow {
            hwnd: 0x1000,
            class: "Notepad".to_string(),
            title: "Untitled - Notepad".to_string(),
            parent: 0,
            x: 80,
            y: 60,
            width: 600,
            height: 400,
            style: 0,
            wndproc: 0x401_aad,
            text: Vec::new(),
            visible: true,
            ..GuestWindow::default()
        }
    }

    fn sample_edit(hwnd: u32, parent: u32, text: &[u8]) -> GuestWindow {
        GuestWindow {
            hwnd,
            class: "Edit".to_string(),
            title: String::new(),
            parent,
            x: 0,
            y: 0,
            width: 500,
            height: 300,
            style: 0,
            wndproc: 0,
            text: text.to_vec(),
            visible: true,
            ..GuestWindow::default()
        }
    }

    fn multiwindow_gui() -> GuiState {
        let mut first = sample_window();
        first.text = b"first".to_vec();
        let mut second = sample_window();
        second.hwnd = 0x2000;
        second.title = "Second - Notepad".to_string();
        second.x = 300;
        second.y = 100;
        let mut gui = GuiState::default();
        gui.windows.push(first);
        gui.windows
            .push(sample_edit(0x1001, 0x1000, b"first window"));
        gui.windows.push(second);
        gui.windows
            .push(sample_edit(0x2001, 0x2000, b"second window"));
        gui.main_hwnd = 0x1000;
        gui.edit_hwnd = 0x2001;
        gui
    }

    #[test]
    fn window_rect_matches_guest_geometry() {
        assert_eq!(
            window_rect(&sample_window()),
            Rect {
                x: 80,
                y: 60,
                width: 600,
                height: 400
            }
        );
    }

    #[test]
    fn title_bar_hit_test_excludes_client_area() {
        let window = sample_window();
        let outer = window_rect(&window);
        let mut gui = GuiState::default();
        gui.windows.push(window);
        gui.main_hwnd = 0x1000;

        let title_x = outer.x + 100;
        let title_y = outer.y + WINDOW_BORDER + 4;
        let client_y = outer.y + WINDOW_BORDER + TITLE_HEIGHT + MENU_HEIGHT + 4;
        assert_eq!(hit_test_window(&gui, title_x, title_y, true), Some(0x1000));
        assert_eq!(hit_test_window(&gui, title_x, client_y, true), None);
        assert_eq!(
            hit_test_window(&gui, title_x, client_y, false),
            Some(0x1000)
        );

        let close = close_button_rect(gui.window(0x1000).unwrap());
        assert_eq!(
            hit_test_close_button(&gui, close.x + close.width / 2, close.y + close.height / 2),
            Some(0x1000)
        );
        assert_eq!(hit_test_close_button(&gui, title_x, title_y), None);
    }

    #[test]
    fn drag_posts_wm_move_with_packed_client_coordinates() {
        assert_eq!(
            move_message(0x1000, 260, 180),
            GuestMessage {
                hwnd: 0x1000,
                message: 0x0003,
                wparam: 0,
                lparam: (180u32 << 16) | 260,
            }
        );
    }

    #[test]
    fn dragged_window_stays_inside_desktop_and_above_taskbar() {
        assert_eq!(clamp_position(-20, -10, 600, 400, 1024, 700), (0, 0));
        assert_eq!(
            clamp_position(900, 600, 600, 400, 1024, 700),
            (424, 700 - TASKBAR_HEIGHT - 400)
        );
        assert_eq!(clamp_position(180, 120, 600, 400, 1024, 700), (180, 120));
    }

    #[test]
    fn client_rect_leaves_room_for_frame_and_menu() {
        let window = sample_window();
        let client = client_rect(&window);
        assert_eq!(client.x, 83);
        assert_eq!(client.y, 60 + WINDOW_BORDER + TITLE_HEIGHT + MENU_HEIGHT);
        assert_eq!(client.width, 600 - WINDOW_BORDER * 2);
        assert_eq!(
            client.height,
            400 - WINDOW_BORDER * 2 - TITLE_HEIGHT - MENU_HEIGHT
        );
    }

    #[test]
    fn each_top_level_window_renders_its_own_edit_text() {
        let gui = multiwindow_gui();
        assert_eq!(gui.client_text_for(0x1000), Some(&b"first window"[..]));
        assert_eq!(gui.client_text_for(0x2000), Some(&b"second window"[..]));
    }

    #[test]
    fn activation_moves_focus_and_z_order_between_windows() {
        let mut gui = multiwindow_gui();
        gui.activate_window(0x1000);
        assert_eq!(gui.active_hwnd, 0x1000);
        assert_eq!(gui.focus_hwnd, 0x1001);
        assert_eq!(gui.windows.last().unwrap().hwnd, 0x1000);
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x1000,
            message: 0x0006,
            wparam: 1,
            lparam: 0,
        }));
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x1001,
            message: 0x0007,
            wparam: 0,
            lparam: 0,
        }));

        gui.activate_window(0x2001);
        assert_eq!(gui.active_hwnd, 0x2000);
        assert_eq!(gui.focus_hwnd, 0x2001);
        assert_eq!(gui.windows.last().unwrap().hwnd, 0x2000);
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x1000,
            message: 0x0006,
            wparam: 0,
            lparam: 0,
        }));
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x1001,
            message: 0x0008,
            wparam: 0,
            lparam: 0,
        }));
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x2000,
            message: 0x0006,
            wparam: 1,
            lparam: 0,
        }));
        assert!(gui.queue.contains(&GuestMessage {
            hwnd: 0x2001,
            message: 0x0007,
            wparam: 0,
            lparam: 0,
        }));
    }

    #[test]
    fn resize_handle_and_wm_size_use_the_window_under_test() {
        let gui = multiwindow_gui();
        let grip = resize_handle_rect(gui.window(0x1000).unwrap());
        assert_eq!(
            hit_test_resize_handle(&gui, grip.x + grip.width / 2, grip.y + grip.height / 2),
            Some(0x1000)
        );
        assert_eq!(
            hit_test_resize_handle(&gui, sample_window().x + 100, sample_window().y + 100),
            None
        );

        let client = client_rect(gui.window(0x1000).unwrap());
        assert_eq!(
            size_message(0x1000, client.width, client.height),
            GuestMessage {
                hwnd: 0x1000,
                message: 0x0005,
                wparam: 0,
                lparam: ((client.height as u16 as u32) << 16) | (client.width as u16 as u32),
            }
        );
    }

    #[test]
    fn taskbar_groups_top_level_windows_by_guest_class() {
        let mut gui = multiwindow_gui();
        assert_eq!(
            process_entries(&gui),
            vec![ProcessEntry {
                name: "Notepad".to_string(),
                title: "Untitled - Notepad".to_string(),
                hwnds: vec![0x1000, 0x2000],
                active: false,
            }]
        );
        gui.active_hwnd = 0x2000;
        assert!(process_entries(&gui)[0].active);
    }

    #[test]
    fn program_windows_render_between_the_desktop_and_the_taskbar() {
        let mut gui = GuiState::default();
        gui.windows.push(sample_window());
        let mut surface = crate::win32::gfx::Surface::new(600, 400, 24);
        surface.pixels.fill(0x123456);
        let mut surfaces = std::collections::HashMap::new();
        surfaces.insert(0x1000, surface);
        let shell = Shell::new(None, Vec::new());
        let tasks = [Task {
            label: "Untitled - Notepad".into(),
            active: true,
            program: "NOTEPAD.EXE".into(),
        }];
        let mut state = SharedState::default();
        state.gui = gui;
        state.gdi.surfaces = surfaces;
        let pixels = render_desktop(1024, 700, &[&state], &shell, &tasks);
        let at = |x: usize, y: usize| {
            let i = (y * 1024 + x) * 3;
            (pixels[i], pixels[i + 1], pixels[i + 2])
        };
        assert_eq!(at(100, 100), (0x12, 0x34, 0x56), "program surface");
        assert_eq!(at(900, 100), (0x00, 0x80, 0x80), "desktop");
        assert_eq!(at(900, 690), (0xC0, 0xC0, 0xC0), "taskbar");
    }
}
