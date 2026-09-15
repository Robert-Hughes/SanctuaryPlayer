//! Direct Android GameActivity host for the shared SanctuaryPlayer core.

#[cfg(target_os = "android")]
mod android {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use android_activity::{
        AndroidApp, InputStatus, MainEvent, PollEvent,
        input::{
            ImeOptions, InputEvent, InputType, KeyAction, KeyMapChar, Keycode, MotionAction,
            TextInputAction, TextInputState, TextSpan,
        },
    };
    use egui::{
        Event, Key, Modifiers, PointerButton, Pos2, RawInput, TouchDeviceId, TouchId, TouchPhase,
    };
    use egui_wgpu::{RendererOptions, ScreenDescriptor};
    use raw_window_handle::{AndroidDisplayHandle, HasWindowHandle};
    use sanctuary_player_app::app::{
        AndroidTextField, AndroidTextInputSnapshot, AppEffect, AppState,
    };
    use sanctuary_player_app::model::AppCommand;
    use sanctuary_player_app::playback::{DecodeMode, PlaybackWake};
    use sanctuary_player_app::video_renderer::VideoRenderer;

    const APP_NAME: &str = "Sanctuary Player";
    const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);
    const SHUTDOWN_POSITION_FLUSH_BUDGET: Duration = Duration::from_secs(2);

    static LOG_INITIALISATION: OnceLock<Result<PathBuf, String>> = OnceLock::new();

    struct RepaintSignal {
        deadline: Mutex<Option<Instant>>,
        waker: android_activity::AndroidAppWaker,
    }

    impl RepaintSignal {
        fn new(app: &AndroidApp) -> Arc<Self> {
            Arc::new(Self {
                deadline: Mutex::new(Some(Instant::now())),
                waker: app.create_waker(),
            })
        }

        fn request(&self, delay: Duration) {
            let deadline = Instant::now() + delay;
            let mut current = self.deadline.lock().expect("repaint deadline poisoned");
            if current.is_none_or(|value| deadline < value) {
                *current = Some(deadline);
            }
            drop(current);
            self.waker.wake();
        }

        fn timeout(&self) -> Option<Duration> {
            let deadline = *self.deadline.lock().expect("repaint deadline poisoned");
            deadline.map(|value| value.saturating_duration_since(Instant::now()))
        }

        fn take_if_due(&self) -> bool {
            let now = Instant::now();
            let mut deadline = self.deadline.lock().expect("repaint deadline poisoned");
            if deadline.is_some_and(|value| value <= now) {
                *deadline = None;
                true
            } else {
                false
            }
        }
    }

    #[derive(Default)]
    struct AndroidInput {
        events: Vec<Event>,
        primary_touch_id: Option<i32>,
        modifiers: Modifiers,
        combining_accent: Option<char>,
        primary_pointer_pressed: bool,
    }

    impl AndroidInput {
        fn push_touch(&mut self, device_id: i32, pointer_id: i32, phase: TouchPhase, pos: Pos2) {
            self.events.push(Event::Touch {
                device_id: TouchDeviceId(device_id.max(0) as u64),
                id: TouchId(pointer_id.max(0) as u64),
                phase,
                pos,
                force: None,
            });
        }

        fn press_primary(&mut self, pointer_id: i32, pos: Pos2) {
            self.primary_pointer_pressed = true;
            self.primary_touch_id = Some(pointer_id);
            self.events.push(Event::PointerMoved(pos));
            self.events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: self.modifiers,
            });
        }

        fn move_primary(&mut self, pointer_id: i32, pos: Pos2) {
            if self.primary_touch_id == Some(pointer_id) {
                self.events.push(Event::PointerMoved(pos));
            }
        }

        fn release_primary(&mut self, pointer_id: i32, pos: Pos2) {
            if self.primary_touch_id == Some(pointer_id) {
                self.events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: self.modifiers,
                });
                self.events.push(Event::PointerGone);
                self.primary_touch_id = None;
            }
        }

        fn cancel_primary(&mut self) {
            if self.primary_touch_id.take().is_some() {
                self.events.push(Event::PointerGone);
            }
        }
    }

    #[derive(Default)]
    struct AndroidIme {
        field: Option<AndroidTextField>,
        text: Option<String>,
        selection: Option<(usize, usize)>,
        compose_region: Option<TextSpan>,
    }

    impl AndroidIme {
        fn update(
            &mut self,
            app: &AndroidApp,
            snapshot: Option<&AndroidTextInputSnapshot>,
            user_tapped: bool,
        ) {
            let Some(snapshot) = snapshot else {
                if user_tapped && self.field.take().is_some() {
                    app.hide_soft_input(false);
                    self.text = None;
                    self.selection = None;
                    self.compose_region = None;
                }
                return;
            };

            let selection = (
                char_index_to_utf16(&snapshot.text, snapshot.selection_start),
                char_index_to_utf16(&snapshot.text, snapshot.selection_end),
            );
            let changed_field = self.field != Some(snapshot.field);
            let changed_state = changed_field
                || self.text.as_deref() != Some(snapshot.text.as_str())
                || self.selection != Some(selection);

            if changed_state {
                self.compose_region = None;
                app.set_text_input_state(TextInputState {
                    text: snapshot.text.clone(),
                    selection: TextSpan {
                        start: selection.0,
                        end: selection.1,
                    },
                    compose_region: None,
                });
                self.text = Some(snapshot.text.clone());
                self.selection = Some(selection);
            }

            if changed_field {
                app.set_ime_editor_info(
                    InputType::TYPE_CLASS_TEXT,
                    TextInputAction::Done,
                    ImeOptions::IME_FLAG_NO_FULLSCREEN,
                );
                app.show_soft_input(true);
            } else if snapshot.clicked {
                app.show_soft_input(false);
            } else if user_tapped {
                // Keep Android's existing input connection when a tap does not
                // move focus away from the currently edited field.
            }

            self.field = Some(snapshot.field);
        }

        fn accept_text_state(&mut self, state: &TextInputState) {
            self.text = Some(state.text.clone());
            self.selection = Some((state.selection.start, state.selection.end));
            self.compose_region = state.compose_region;
        }
    }

    struct AndroidGpu {
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: Option<wgpu::Surface<'static>>,
        surface_config: Option<wgpu::SurfaceConfiguration>,
        surface_format: wgpu::TextureFormat,
        egui_renderer: egui_wgpu::Renderer,
        video_renderer: VideoRenderer,
    }

    impl AndroidGpu {
        fn new(app: &AndroidApp) -> Result<Self, String> {
            let native_window = app
                .native_window()
                .ok_or_else(|| "Android native window is unavailable".to_owned())?;
            let width = native_window.width().max(1) as u32;
            let height = native_window.height().max(1) as u32;
            let instance = wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            );
            let surface = create_surface(&instance, &native_window)?;
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: Some(&surface),
                }))
                .map_err(|error| format!("could not find Android GPU adapter: {error}"))?;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("sanctuary-player-android-device"),
                    ..Default::default()
                }))
                .map_err(|error| format!("could not create Android GPU device: {error}"))?;
            let mut config = surface
                .get_default_config(&adapter, width, height)
                .ok_or_else(|| {
                    "Android surface is incompatible with the selected GPU".to_owned()
                })?;
            config.present_mode = wgpu::PresentMode::AutoVsync;
            config.desired_maximum_frame_latency = 1;
            let surface_format = config.format;
            surface.configure(&device, &config);

            let max_texture_dimension_2d = device.limits().max_texture_dimension_2d;
            let egui_renderer =
                egui_wgpu::Renderer::new(&device, surface_format, RendererOptions::default());
            let video_renderer =
                VideoRenderer::new(&device, &queue, surface_format, max_texture_dimension_2d);

            log::info!(
                "SanctuaryPlayer: Android GPU initialised adapter={} backend={:?} format={:?} size={}x{}",
                adapter.get_info().name,
                adapter.get_info().backend,
                surface_format,
                width,
                height
            );

            Ok(Self {
                instance,
                adapter,
                device,
                queue,
                surface: Some(surface),
                surface_config: Some(config),
                surface_format,
                egui_renderer,
                video_renderer,
            })
        }

        fn attach_window(&mut self, app: &AndroidApp) -> Result<(), String> {
            let native_window = app
                .native_window()
                .ok_or_else(|| "Android native window is unavailable".to_owned())?;
            let width = native_window.width().max(1) as u32;
            let height = native_window.height().max(1) as u32;
            let surface = create_surface(&self.instance, &native_window)?;
            let capabilities = surface.get_capabilities(&self.adapter);
            if !capabilities.formats.contains(&self.surface_format) {
                return Err(format!(
                    "recreated Android surface no longer supports {:?}",
                    self.surface_format
                ));
            }
            let mut config = surface
                .get_default_config(&self.adapter, width, height)
                .ok_or_else(|| "recreated Android surface is incompatible".to_owned())?;
            config.format = self.surface_format;
            config.present_mode = wgpu::PresentMode::AutoVsync;
            config.desired_maximum_frame_latency = 1;
            surface.configure(&self.device, &config);
            self.surface = Some(surface);
            self.surface_config = Some(config);
            Ok(())
        }

        fn detach_window(&mut self) {
            self.surface = None;
            self.surface_config = None;
        }

        fn resize_from_app(&mut self, app: &AndroidApp) -> Result<(), String> {
            let native_window = app
                .native_window()
                .ok_or_else(|| "Android native window is unavailable".to_owned())?;
            let Some(surface) = self.surface.as_ref() else {
                return Ok(());
            };
            let mut config = self
                .surface_config
                .clone()
                .ok_or_else(|| "Android surface configuration is unavailable".to_owned())?;
            config.width = native_window.width().max(1) as u32;
            config.height = native_window.height().max(1) as u32;
            surface.configure(&self.device, &config);
            self.surface_config = Some(config);
            Ok(())
        }

        fn dimensions(&self) -> Option<[u32; 2]> {
            self.surface_config
                .as_ref()
                .map(|config| [config.width, config.height])
        }

        fn paint(
            &mut self,
            app: &AndroidApp,
            ctx: &egui::Context,
            state: &mut AppState,
            raw_input: RawInput,
        ) -> Result<Option<(egui::FullOutput, Vec<AppCommand>)>, String> {
            let Some(surface) = self.surface.as_ref() else {
                return Ok(None);
            };
            let Some(config) = self.surface_config.as_ref() else {
                return Ok(None);
            };
            let output = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(output) => output,
                wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    self.attach_window(app)?;
                    return Ok(None);
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(None);
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    return Err("Android surface acquisition failed validation".into());
                }
            };
            let target = output
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("sanctuary-player-android-frame"),
                });

            if !state.has_video() {
                self.video_renderer.reset();
            }
            let decode_mode = state.decode_mode();
            if let Some(frame) = state.take_video_frame_lease() {
                self.video_renderer
                    .upload_lease(&self.device, &self.queue, &frame, decode_mode)?;
            }
            self.video_renderer.draw(
                &self.queue,
                &mut encoder,
                &target,
                config.width,
                config.height,
            );

            let mut commands = Vec::new();
            let full_output = ctx.run_ui(raw_input, |root_ui| {
                commands = sanctuary_player_app::ui::render(root_ui, state);
            });
            for (texture_id, image_delta) in &full_output.textures_delta.set {
                self.egui_renderer.update_texture(
                    &self.device,
                    &self.queue,
                    *texture_id,
                    image_delta,
                );
            }
            let paint_jobs =
                ctx.tessellate(full_output.shapes.clone(), full_output.pixels_per_point);
            let screen = ScreenDescriptor {
                size_in_pixels: [config.width, config.height],
                pixels_per_point: full_output.pixels_per_point,
            };
            self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                &paint_jobs,
                &screen,
            );
            {
                let mut render_pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("sanctuary-player-android-egui"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &target,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();
                self.egui_renderer
                    .render(&mut render_pass, &paint_jobs, &screen);
            }
            for texture_id in &full_output.textures_delta.free {
                self.egui_renderer.free_texture(texture_id);
            }
            self.queue.submit([encoder.finish()]);
            output.present();

            Ok(Some((full_output, commands)))
        }
    }

    #[allow(unsafe_code)]
    fn create_surface(
        instance: &wgpu::Instance,
        native_window: &impl HasWindowHandle,
    ) -> Result<wgpu::Surface<'static>, String> {
        let raw_window_handle = native_window
            .window_handle()
            .map_err(|error| format!("could not get Android window handle: {error}"))?
            .as_raw();
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(AndroidDisplayHandle::new().into()),
                raw_window_handle,
            })
        }
        .map_err(|error| format!("could not create Android wgpu surface: {error}"))
    }

    fn native_pixels_per_point(app: &AndroidApp) -> f32 {
        app.config()
            .density()
            .map_or(1.0, |dpi| dpi as f32 / 160.0)
            .max(1.0)
    }

    fn make_raw_input(
        app: &AndroidApp,
        ctx: &egui::Context,
        input: &mut AndroidInput,
        focused: bool,
        size_in_pixels: [u32; 2],
        started: Instant,
    ) -> RawInput {
        let native_ppp = native_pixels_per_point(app);
        let pixels_per_point = native_ppp * ctx.zoom_factor();
        let screen_rect = egui::Rect::from_min_size(
            Pos2::ZERO,
            egui::vec2(
                size_in_pixels[0] as f32 / pixels_per_point,
                size_in_pixels[1] as f32 / pixels_per_point,
            ),
        );
        let content = app.content_rect();
        let right_inset = (size_in_pixels[0] as i32 - content.right).max(0);
        let bottom_inset = (size_in_pixels[1] as i32 - content.bottom).max(0);

        let mut raw = RawInput {
            screen_rect: Some(screen_rect),
            safe_area_insets: Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
                left: content.left.max(0) as f32 / pixels_per_point,
                top: content.top.max(0) as f32 / pixels_per_point,
                right: right_inset as f32 / pixels_per_point,
                bottom: bottom_inset as f32 / pixels_per_point,
            })),
            time: Some(started.elapsed().as_secs_f64()),
            focused,
            modifiers: input.modifiers,
            events: std::mem::take(&mut input.events),
            ..Default::default()
        };
        if let Some(viewport) = raw.viewports.get_mut(&raw.viewport_id) {
            viewport.title = Some(APP_NAME.to_owned());
            viewport.native_pixels_per_point = Some(native_ppp);
            viewport.inner_rect = Some(screen_rect);
            viewport.focused = Some(focused);
        }
        raw
    }

    fn modifiers(meta: android_activity::input::MetaState) -> Modifiers {
        Modifiers {
            alt: meta.alt_on(),
            ctrl: meta.ctrl_on(),
            shift: meta.shift_on(),
            mac_cmd: false,
            command: meta.ctrl_on(),
        }
    }

    fn key_map_character(
        app: &AndroidApp,
        event: &android_activity::input::KeyEvent<'_>,
        combining_accent: &mut Option<char>,
    ) -> Option<char> {
        if event.device_id() == 0 {
            return None;
        }
        let map = app.device_key_character_map(event.device_id()).ok()?;
        match map.get(event.key_code(), event.meta_state()) {
            Ok(KeyMapChar::Unicode(ch)) => {
                if event.action() != KeyAction::Down {
                    return Some(ch);
                }
                if let Some(accent) = combining_accent.take() {
                    map.get_dead_char(accent, ch).ok().flatten().or(Some(ch))
                } else {
                    Some(ch)
                }
            }
            Ok(KeyMapChar::CombiningAccent(accent)) => {
                if event.action() == KeyAction::Down {
                    *combining_accent = Some(accent);
                }
                None
            }
            Ok(KeyMapChar::None) | Err(_) => None,
        }
    }

    fn egui_key(key: Keycode) -> Option<Key> {
        match key {
            Keycode::DpadUp => Some(Key::ArrowUp),
            Keycode::DpadDown => Some(Key::ArrowDown),
            Keycode::DpadLeft => Some(Key::ArrowLeft),
            Keycode::DpadRight => Some(Key::ArrowRight),
            Keycode::Escape => Some(Key::Escape),
            Keycode::Tab => Some(Key::Tab),
            Keycode::Enter | Keycode::NumpadEnter => Some(Key::Enter),
            Keycode::Space => Some(Key::Space),
            Keycode::Del => Some(Key::Backspace),
            Keycode::ForwardDel => Some(Key::Delete),
            Keycode::MoveHome => Some(Key::Home),
            Keycode::MoveEnd => Some(Key::End),
            Keycode::PageUp => Some(Key::PageUp),
            Keycode::PageDown => Some(Key::PageDown),
            _ => None,
        }
    }

    fn drain_input(
        app: &AndroidApp,
        input: &mut AndroidInput,
        ime: &mut AndroidIme,
        state: &mut AppState,
        pixels_per_point: f32,
    ) {
        let Ok(mut iter) = app.input_events_iter() else {
            return;
        };
        while iter.next(|event| match event {
            InputEvent::MotionEvent(event) => {
                let device_id = event.device_id();
                match event.action() {
                    MotionAction::Down | MotionAction::PointerDown => {
                        let pointer = event.pointer_at_index(event.pointer_index());
                        let id = pointer.pointer_id();
                        let pos = Pos2::new(
                            pointer.x() / pixels_per_point,
                            pointer.y() / pixels_per_point,
                        );
                        input.push_touch(device_id, id, TouchPhase::Start, pos);
                        if input.primary_touch_id.is_none() {
                            input.press_primary(id, pos);
                        }
                    }
                    MotionAction::Move => {
                        for pointer in event.pointers() {
                            let id = pointer.pointer_id();
                            let pos = Pos2::new(
                                pointer.x() / pixels_per_point,
                                pointer.y() / pixels_per_point,
                            );
                            input.push_touch(device_id, id, TouchPhase::Move, pos);
                            input.move_primary(id, pos);
                        }
                    }
                    MotionAction::Up | MotionAction::PointerUp => {
                        let pointer = event.pointer_at_index(event.pointer_index());
                        let id = pointer.pointer_id();
                        let pos = Pos2::new(
                            pointer.x() / pixels_per_point,
                            pointer.y() / pixels_per_point,
                        );
                        input.push_touch(device_id, id, TouchPhase::End, pos);
                        input.release_primary(id, pos);
                    }
                    MotionAction::Cancel => {
                        for pointer in event.pointers() {
                            let id = pointer.pointer_id();
                            let pos = Pos2::new(
                                pointer.x() / pixels_per_point,
                                pointer.y() / pixels_per_point,
                            );
                            input.push_touch(device_id, id, TouchPhase::Cancel, pos);
                        }
                        input.cancel_primary();
                    }
                    _ => {}
                }
                InputStatus::Handled
            }
            InputEvent::KeyEvent(event) => {
                if event.key_code() == Keycode::Back {
                    return InputStatus::Unhandled;
                }
                input.modifiers = modifiers(event.meta_state());
                let mapped_character = key_map_character(app, event, &mut input.combining_accent);
                let key = egui_key(event.key_code())
                    .or_else(|| mapped_character.and_then(|ch| Key::from_name(&ch.to_string())));
                let pressed = event.action() == KeyAction::Down;
                if let Some(key) = key {
                    input.events.push(Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: pressed && event.repeat_count() > 0,
                        modifiers: input.modifiers,
                    });
                }
                if pressed
                    && !input.modifiers.ctrl
                    && !input.modifiers.command
                    && !input.modifiers.mac_cmd
                    && let Some(ch) = mapped_character
                    && !ch.is_control()
                {
                    input.events.push(Event::Text(ch.to_string()));
                }
                if key.is_some() || mapped_character.is_some() {
                    InputStatus::Handled
                } else {
                    InputStatus::Unhandled
                }
            }
            InputEvent::TextEvent(text_state) => {
                ime.accept_text_state(text_state);
                state.apply_android_text_input_state(
                    text_state.text.clone(),
                    utf16_index_to_char(&text_state.text, text_state.selection.start),
                    utf16_index_to_char(&text_state.text, text_state.selection.end),
                );
                InputStatus::Handled
            }
            InputEvent::TextAction(_) => {
                input.events.push(Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: input.modifiers,
                });
                input.events.push(Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: input.modifiers,
                });
                InputStatus::Handled
            }
            _ => InputStatus::Unhandled,
        }) {}
    }

    fn handle_platform_output(
        app: &AndroidApp,
        state: &AppState,
        output: &egui::PlatformOutput,
        ime: &mut AndroidIme,
        user_tapped: bool,
    ) {
        ime.update(app, state.android_text_input(), user_tapped);
        for command in &output.commands {
            match command {
                egui::OutputCommand::CopyText(_) => {
                    log::info!("SanctuaryPlayer: Android clipboard output is not implemented yet");
                }
                egui::OutputCommand::CopyImage(_) | egui::OutputCommand::OpenUrl(_) => {}
            }
        }
    }

    fn char_index_to_utf16(text: &str, char_index: usize) -> usize {
        text.chars().take(char_index).map(char::len_utf16).sum()
    }

    fn utf16_index_to_char(text: &str, utf16_index: usize) -> usize {
        let mut units = 0;
        for (index, ch) in text.chars().enumerate() {
            if units >= utf16_index {
                return index;
            }
            units += ch.len_utf16();
            if units >= utf16_index {
                return index + 1;
            }
        }
        text.chars().count()
    }

    fn min_timeout(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }

    fn state_timeout(state: &AppState, now: Instant) -> Option<Duration> {
        let mut timeout = state.needs_animation().then_some(ANIMATION_FRAME_INTERVAL);
        timeout = min_timeout(
            timeout,
            state
                .playback_wake_deadline(now)
                .map(|deadline| deadline.saturating_duration_since(now)),
        );
        min_timeout(
            timeout,
            state
                .persistence_wake_deadline(now)
                .map(|deadline| deadline.saturating_duration_since(now)),
        )
    }

    fn state_due(state: &AppState, now: Instant) -> bool {
        state.needs_animation()
            || state
                .playback_wake_deadline(now)
                .is_some_and(|deadline| deadline <= now)
            || state
                .persistence_wake_deadline(now)
                .is_some_and(|deadline| deadline <= now)
    }

    fn apply_commands(state: &mut AppState, commands: Vec<AppCommand>) {
        for command in commands {
            if matches!(state.apply(command), Some(AppEffect::ToggleFullscreen)) {
                log::info!("SanctuaryPlayer: fullscreen command ignored on Android GameActivity");
            }
        }
    }

    fn initialise_logging(internal_data_path: &std::path::Path) {
        let result = LOG_INITIALISATION.get_or_init(|| {
            sanctuary_player_app::logging::init(internal_data_path.join("logs"))
                .map_err(|error| error.to_string())
        });
        if let Err(error) = result {
            eprintln!("SanctuaryPlayer: failed to initialise Android logging: {error}");
        }
    }

    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub fn android_main(android_app: AndroidApp) {
        let Some(internal_data_path) = android_app.internal_data_path() else {
            eprintln!("SanctuaryPlayer: Android internal data path is unavailable");
            return;
        };
        initialise_logging(&internal_data_path);
        log::info!("SanctuaryPlayer: Android GameActivity starting");

        let ctx = egui::Context::default();
        sanctuary_player_app::ui::theme::configure_context(&ctx);
        let repaint = RepaintSignal::new(&android_app);
        let repaint_callback = Arc::clone(&repaint);
        ctx.set_request_repaint_callback(move |request| {
            repaint_callback.request(request.delay);
        });

        let mut state = AppState::with_decode_mode(DecodeMode::Cpu);
        state.set_muted(true);
        state.set_settings_path(internal_data_path.join("settings.json"));
        state.set_session_path(internal_data_path.join("session.json"));
        let playback_waker = android_app.create_waker();
        state.set_playback_wake(PlaybackWake::new(move || {
            playback_waker.wake();
        }));
        if let Some(source) = state.take_startup_session_source() {
            let _ = state.apply(AppCommand::OpenVideo(source));
        }

        let started = Instant::now();
        let mut last_update = started;
        let mut gpu: Option<AndroidGpu> = None;
        let mut input = AndroidInput::default();
        let mut ime = AndroidIme::default();
        let mut focused = false;
        let mut input_available = false;
        let mut running = true;

        while running {
            let now = Instant::now();
            let timeout = min_timeout(repaint.timeout(), state_timeout(&state, now));
            android_app.poll_events(timeout, |event| match event {
                PollEvent::Main(MainEvent::Destroy) => {
                    log::info!("SanctuaryPlayer: Android GameActivity destroyed");
                    state.flush_persistence_for_shutdown(SHUTDOWN_POSITION_FLUSH_BUDGET);
                    gpu = None;
                    running = false;
                }
                PollEvent::Main(MainEvent::InitWindow { .. }) => {
                    let result = match gpu.as_mut() {
                        Some(gpu) => gpu.attach_window(&android_app),
                        None => AndroidGpu::new(&android_app).map(|created| {
                            gpu = Some(created);
                        }),
                    };
                    if let Err(error) = result {
                        log::error!(
                            "SanctuaryPlayer: could not attach Android render surface: {error}"
                        );
                    }
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
                    if let Some(gpu) = gpu.as_mut() {
                        gpu.detach_window();
                    }
                }
                PollEvent::Main(MainEvent::WindowResized { .. }) => {
                    if let Some(gpu) = gpu.as_mut()
                        && let Err(error) = gpu.resize_from_app(&android_app)
                    {
                        log::error!("SanctuaryPlayer: could not resize Android surface: {error}");
                    }
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::InputAvailable) => {
                    input_available = true;
                }
                PollEvent::Main(MainEvent::GainedFocus) => {
                    focused = true;
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::LostFocus) => {
                    focused = false;
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(
                    MainEvent::Pause | MainEvent::Stop | MainEvent::SaveState { .. },
                ) => {
                    state.flush_persistence_for_background();
                }
                PollEvent::Main(MainEvent::RedrawNeeded { .. })
                | PollEvent::Main(MainEvent::ContentRectChanged { .. })
                | PollEvent::Main(MainEvent::InsetsChanged { .. })
                | PollEvent::Main(MainEvent::ConfigChanged { .. })
                | PollEvent::Main(MainEvent::Resume { .. })
                | PollEvent::Main(MainEvent::Start) => {
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Wake => {
                    let _ = state.take_playback_wakes();
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::LowMemory) | PollEvent::Timeout => {}
                _ => {}
            });

            if !running {
                break;
            }

            let pixels_per_point = native_pixels_per_point(&android_app) * ctx.zoom_factor();
            if input_available {
                drain_input(
                    &android_app,
                    &mut input,
                    &mut ime,
                    &mut state,
                    pixels_per_point,
                );
                input_available = false;
                repaint.request(Duration::ZERO);
            }

            let now = Instant::now();
            if state_due(&state, now) {
                repaint.request(Duration::ZERO);
            }

            if repaint.take_if_due()
                && let Some(gpu) = gpu.as_mut()
                && let Some(size) = gpu.dimensions()
            {
                state.update(now.saturating_duration_since(last_update));
                last_update = now;
                let user_tapped = std::mem::take(&mut input.primary_pointer_pressed);
                let raw_input =
                    make_raw_input(&android_app, &ctx, &mut input, focused, size, started);
                match gpu.paint(&android_app, &ctx, &mut state, raw_input) {
                    Ok(Some((output, commands))) => {
                        handle_platform_output(
                            &android_app,
                            &state,
                            &output.platform_output,
                            &mut ime,
                            user_tapped,
                        );
                        if !commands.is_empty() {
                            apply_commands(&mut state, commands);
                            repaint.request(Duration::ZERO);
                        }
                        if let Some(delay) = output
                            .viewport_output
                            .get(&egui::ViewportId::ROOT)
                            .map(|viewport| viewport.repaint_delay)
                            .filter(|delay| *delay != Duration::MAX)
                        {
                            repaint.request(delay);
                        }
                    }
                    Ok(None) => repaint.request(ANIMATION_FRAME_INTERVAL),
                    Err(error) => {
                        log::error!("SanctuaryPlayer: Android render failed: {error}");
                        repaint.request(ANIMATION_FRAME_INTERVAL);
                    }
                }
            }
        }

        log::info!("SanctuaryPlayer: Android activity loop exited");
        log::logger().flush();
    }
}

#[cfg(target_os = "android")]
pub use android::android_main;
