//! Direct Android GameActivity host for the shared SanctuaryPlayer core.

#[cfg(target_os = "android")]
mod android {
    use std::path::PathBuf;
    use std::sync::{
        Arc, Mutex, OnceLock,
        mpsc::{self, Receiver, Sender},
    };
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
    use sanctuary_player_app::model::{AppCommand, PlaybackState};
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

    #[derive(Clone, Copy, Debug)]
    enum AndroidMediaEvent {
        AudioFocusChange(i32),
        BecomingNoisy,
    }

    struct AndroidMediaEventBridge {
        sender: Sender<AndroidMediaEvent>,
        waker: android_activity::AndroidAppWaker,
    }

    static MEDIA_EVENT_BRIDGE: OnceLock<Mutex<Option<AndroidMediaEventBridge>>> = OnceLock::new();

    fn media_event_bridge() -> &'static Mutex<Option<AndroidMediaEventBridge>> {
        MEDIA_EVENT_BRIDGE.get_or_init(|| Mutex::new(None))
    }

    fn install_media_event_bridge(sender: Sender<AndroidMediaEvent>, app: &AndroidApp) {
        *media_event_bridge()
            .lock()
            .expect("Android media event bridge poisoned") = Some(AndroidMediaEventBridge {
            sender,
            waker: app.create_waker(),
        });
    }

    fn uninstall_media_event_bridge() {
        *media_event_bridge()
            .lock()
            .expect("Android media event bridge poisoned") = None;
    }

    fn enqueue_media_event(event: AndroidMediaEvent) {
        let bridge = media_event_bridge()
            .lock()
            .expect("Android media event bridge poisoned");
        let Some(bridge) = bridge.as_ref() else {
            return;
        };
        if bridge.sender.send(event).is_ok() {
            bridge.waker.wake();
        }
    }

    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub unsafe extern "system" fn Java_app_sanctuaryplayer_android_SanctuaryPlayerActivity_nativeOnAudioFocusChange(
        _env: *mut jni::sys::JNIEnv,
        _class: jni::sys::jclass,
        focus_change: jni::sys::jint,
    ) {
        enqueue_media_event(AndroidMediaEvent::AudioFocusChange(focus_change));
    }

    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub unsafe extern "system" fn Java_app_sanctuaryplayer_android_SanctuaryPlayerActivity_nativeOnBecomingNoisy(
        _env: *mut jni::sys::JNIEnv,
        _class: jni::sys::jclass,
    ) {
        enqueue_media_event(AndroidMediaEvent::BecomingNoisy);
    }

    #[allow(unsafe_code)]
    fn android_initialise_media_integration(app: &AndroidApp) -> Result<(), String> {
        use jni::{JavaVM, jni_sig, jni_str, objects::JObject};

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            env.call_method(
                &activity,
                jni_str!("initialiseMediaIntegration"),
                jni_sig!("()V"),
                &[],
            )?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    #[allow(unsafe_code)]
    fn android_request_audio_focus(app: &AndroidApp) -> Result<bool, String> {
        use jni::{JavaVM, jni_sig, jni_str, objects::JObject};

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<bool> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            env.call_method(
                &activity,
                jni_str!("requestPlaybackAudioFocus"),
                jni_sig!("()Z"),
                &[],
            )?
            .z()
        })
        .map_err(|error| error.to_string())
    }

    #[allow(unsafe_code)]
    fn android_abandon_audio_focus(app: &AndroidApp) -> Result<(), String> {
        use jni::{JavaVM, jni_sig, jni_str, objects::JObject};

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            env.call_method(
                &activity,
                jni_str!("abandonPlaybackAudioFocus"),
                jni_sig!("()V"),
                &[],
            )?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    #[allow(unsafe_code)]
    fn android_shutdown_media_integration(app: &AndroidApp) -> Result<(), String> {
        use jni::{JavaVM, jni_sig, jni_str, objects::JObject};

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            env.call_method(
                &activity,
                jni_str!("shutdownMediaIntegration"),
                jni_sig!("()V"),
                &[],
            )?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    #[allow(unsafe_code)]
    fn android_set_playback_keeps_screen_on(
        app: &AndroidApp,
        keep_screen_on: bool,
    ) -> Result<(), String> {
        use jni::{
            JavaVM, jni_sig, jni_str,
            objects::{JObject, JValue},
        };

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            env.call_method(
                &activity,
                jni_str!("setPlaybackKeepsScreenOn"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(keep_screen_on)],
            )?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    #[derive(Default)]
    struct AndroidScreenOn {
        enabled: bool,
        foreground: bool,
    }

    impl AndroidScreenOn {
        fn set_foreground(&mut self, app: &AndroidApp, state: &AppState, foreground: bool) {
            self.foreground = foreground;
            self.sync(app, state);
        }

        fn sync(&mut self, app: &AndroidApp, state: &AppState) {
            let desired = self.foreground
                && match state.playback_state() {
                    PlaybackState::Playing => true,
                    // Preserve the previous state across a seek: a seek that began
                    // while playing should not briefly allow display sleep, while a
                    // seek from a paused session should not acquire the inhibition.
                    PlaybackState::Seeking => self.enabled,
                    _ => false,
                };
            if desired == self.enabled {
                return;
            }

            match android_set_playback_keeps_screen_on(app, desired) {
                Ok(()) => {
                    self.enabled = desired;
                    log::info!("SanctuaryPlayer: Android keep-screen-on={desired}");
                }
                Err(error) => {
                    log::error!(
                        "SanctuaryPlayer: could not set Android keep-screen-on={desired}: {error}"
                    );
                }
            }
        }
    }

    #[derive(Default)]
    struct AndroidMediaFocus {
        request_active: bool,
        has_focus: bool,
        resume_after_transient_loss: bool,
    }

    impl AndroidMediaFocus {
        fn ensure_focus(&mut self, app: &AndroidApp) -> bool {
            if self.request_active {
                return self.has_focus;
            }
            match android_request_audio_focus(app) {
                Ok(true) => {
                    self.request_active = true;
                    self.has_focus = true;
                    log::info!("SanctuaryPlayer: Android audio focus granted");
                    true
                }
                Ok(false) => {
                    log::warn!("SanctuaryPlayer: Android audio focus request denied");
                    false
                }
                Err(error) => {
                    log::error!("SanctuaryPlayer: Android audio focus request failed: {error}");
                    false
                }
            }
        }

        fn abandon(&mut self, app: &AndroidApp) {
            if self.request_active
                && let Err(error) = android_abandon_audio_focus(app)
            {
                log::error!("SanctuaryPlayer: could not abandon Android audio focus: {error}");
            }
            self.request_active = false;
            self.has_focus = false;
            self.resume_after_transient_loss = false;
        }

        fn sync_playback_state(&mut self, app: &AndroidApp, state: &mut AppState) {
            match state.playback_state() {
                PlaybackState::Playing => {
                    if !self.ensure_focus(app) {
                        state.pause_for_platform_interruption();
                    }
                }
                PlaybackState::Seeking if self.request_active => {}
                PlaybackState::Paused if self.resume_after_transient_loss => {}
                _ => self.abandon(app),
            }
        }

        fn handle_event(
            &mut self,
            app: &AndroidApp,
            state: &mut AppState,
            event: AndroidMediaEvent,
        ) {
            const AUDIOFOCUS_GAIN: i32 = 1;
            const AUDIOFOCUS_LOSS: i32 = -1;
            const AUDIOFOCUS_LOSS_TRANSIENT: i32 = -2;
            const AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK: i32 = -3;

            match event {
                AndroidMediaEvent::AudioFocusChange(AUDIOFOCUS_GAIN) => {
                    if !self.request_active {
                        log::info!("SanctuaryPlayer: ignoring stale Android audio-focus gain");
                        return;
                    }
                    log::info!("SanctuaryPlayer: Android audio focus gained");
                    self.has_focus = true;
                    if std::mem::take(&mut self.resume_after_transient_loss) {
                        state.resume_after_platform_interruption();
                    }
                }
                AndroidMediaEvent::AudioFocusChange(AUDIOFOCUS_LOSS) => {
                    if !self.request_active {
                        log::info!("SanctuaryPlayer: ignoring stale Android audio-focus loss");
                        return;
                    }
                    log::info!("SanctuaryPlayer: Android audio focus lost permanently");
                    state.pause_for_platform_interruption();
                    self.abandon(app);
                }
                AndroidMediaEvent::AudioFocusChange(
                    AUDIOFOCUS_LOSS_TRANSIENT | AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK,
                ) => {
                    if !self.request_active {
                        log::info!(
                            "SanctuaryPlayer: ignoring stale transient Android audio-focus loss"
                        );
                        return;
                    }
                    log::info!("SanctuaryPlayer: Android audio focus lost transiently; pausing");
                    let was_active = state.pause_for_platform_interruption();
                    self.resume_after_transient_loss |= was_active;
                    self.has_focus = false;
                }
                AndroidMediaEvent::AudioFocusChange(other) => {
                    log::warn!("SanctuaryPlayer: unrecognised Android audio focus change {other}");
                }
                AndroidMediaEvent::BecomingNoisy => {
                    log::info!("SanctuaryPlayer: Android audio route becoming noisy; pausing");
                    state.pause_for_platform_interruption();
                    self.abandon(app);
                }
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

    fn mediacodec_direct_gpu_available(device: &wgpu::Device) -> bool {
        if !device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return false;
        }
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else {
            return false;
        };
        let extensions = hal.enabled_device_extensions();
        let ahb = extensions
            .iter()
            .any(|name| name.to_bytes() == b"VK_ANDROID_external_memory_android_hardware_buffer");
        let foreign = extensions
            .iter()
            .any(|name| name.to_bytes() == b"VK_EXT_queue_family_foreign");
        ahb && foreign
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
            let mut required_features = wgpu::Features::empty();
            if adapter
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
            {
                required_features |= wgpu::Features::TEXTURE_FORMAT_NV12;
            }
            let device_descriptor = wgpu::DeviceDescriptor {
                label: Some("sanctuary-player-android-device"),
                required_features,
                ..Default::default()
            };

            // MediaCodec direct presentation needs two Android-specific Vulkan device
            // extensions plus sampler-YCbCr conversion. Keep those application-specific
            // requirements out of wgpu-hal: its Vulkan adapter exposes a device-creation
            // callback precisely so native interop users can extend VkDeviceCreateInfo
            // before wrapping the resulting HAL device back into an ordinary wgpu
            // Device/Queue.
            let mut sampler_ycbcr =
                ash::vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
                    .sampler_ycbcr_conversion(true);
            let custom_open = if required_features.contains(wgpu::Features::TEXTURE_FORMAT_NV12) {
                let hal_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() };
                hal_adapter.and_then(|hal_adapter| {
                    let caps = hal_adapter.physical_device_capabilities();
                    let ahb = c"VK_ANDROID_external_memory_android_hardware_buffer";
                    let foreign = c"VK_EXT_queue_family_foreign";
                    if !caps.supports_extension(ahb) || !caps.supports_extension(foreign) {
                        return None;
                    }
                    Some(unsafe {
                        hal_adapter.open_with_callback(
                            device_descriptor.required_features,
                            &device_descriptor.required_limits,
                            &device_descriptor.memory_hints,
                            Some(Box::new(|args| {
                                args.extensions.push(ahb);
                                args.extensions.push(foreign);
                                *args.create_info = args.create_info.push_next(&mut sampler_ycbcr);
                            })),
                        )
                    })
                })
            } else {
                None
            };

            let (device, queue) = match custom_open {
                Some(Ok(open)) => {
                    log::info!(
                        "SanctuaryPlayer: Android Vulkan device created with MediaCodec interop extensions"
                    );
                    unsafe {
                        adapter
                            .create_device_from_hal(open, &device_descriptor)
                            .map_err(|error| {
                                format!("could not wrap Android Vulkan device: {error}")
                            })?
                    }
                }
                Some(Err(error)) => {
                    log::warn!(
                        "SanctuaryPlayer: custom Android Vulkan device creation failed ({error}); using ordinary wgpu device"
                    );
                    pollster::block_on(adapter.request_device(&device_descriptor))
                        .map_err(|error| format!("could not create Android GPU device: {error}"))?
                }
                None => pollster::block_on(adapter.request_device(&device_descriptor))
                    .map_err(|error| format!("could not create Android GPU device: {error}"))?,
            };
            let direct_media = mediacodec_direct_gpu_available(&device)
                && option_env!("SANCTUARY_ANDROID_DISABLE_MEDIACODEC_DIRECT").is_none();
            oxideav_mediacodec::set_direct_presentation_available(direct_media);
            log::info!(
                "SanctuaryPlayer: MediaCodec direct Vulkan presentation available={direct_media}{}",
                if option_env!("SANCTUARY_ANDROID_DISABLE_MEDIACODEC_DIRECT").is_some() {
                    " (disabled by validation build)"
                } else {
                    ""
                }
            );
            let mut config = surface
                .get_default_config(&adapter, width, height)
                .ok_or_else(|| {
                    "Android surface is incompatible with the selected GPU".to_owned()
                })?;
            let capabilities = surface.get_capabilities(&adapter);
            if let Some(format) = capabilities.formats.iter().copied().find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            }) {
                // The YUV shader produces video R'G'B' values, which are already
                // transfer-encoded. Rendering them into an sRGB attachment would
                // apply an additional linear-to-sRGB transform and wash out video.
                config.format = format;
            }
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
            let color = state.video_color_info();
            if let Some(frame) = state.take_video_frame_lease() {
                self.video_renderer.upload_lease(
                    &self.device,
                    &self.queue,
                    &frame,
                    decode_mode,
                    color,
                )?;
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
                let viewport = ctx.viewport_rect();
                let content = ctx.content_rect();
                let fill = root_ui.visuals().panel_fill;
                let painter = egui::Painter::new(
                    ctx.clone(),
                    egui::LayerId::new(
                        egui::Order::Foreground,
                        egui::Id::new("android-system-chrome-background"),
                    ),
                    viewport,
                );
                let system_chrome_rects = [
                    egui::Rect::from_min_max(
                        viewport.min,
                        egui::pos2(viewport.max.x, content.min.y),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(viewport.min.x, content.max.y),
                        viewport.max,
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(viewport.min.x, content.min.y),
                        egui::pos2(content.min.x, content.max.y),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(content.max.x, content.min.y),
                        egui::pos2(viewport.max.x, content.max.y),
                    ),
                ];
                for rect in system_chrome_rects {
                    if rect.width() > 0.0 && rect.height() > 0.0 {
                        painter.rect_filled(rect, 0.0, fill);
                    }
                }

                root_ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
                    commands = sanctuary_player_app::ui::render(ui, state);
                });
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct AndroidSystemInsets {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[allow(unsafe_code)]
    fn android_system_window_insets(
        app: &AndroidApp,
        fullscreen: bool,
    ) -> Result<Option<AndroidSystemInsets>, String> {
        use jni::{
            JavaVM, jni_sig, jni_str,
            objects::{JObject, JValue},
        };

        if fullscreen {
            return Ok(Some(AndroidSystemInsets {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            }));
        }

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<Option<AndroidSystemInsets>> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            let window = env
                .call_method(
                    &activity,
                    jni_str!("getWindow"),
                    jni_sig!("()Landroid/view/Window;"),
                    &[],
                )?
                .l()?;
            let decor = env
                .call_method(
                    &window,
                    jni_str!("getDecorView"),
                    jni_sig!("()Landroid/view/View;"),
                    &[],
                )?
                .l()?;
            let insets = env
                .call_method(
                    &decor,
                    jni_str!("getRootWindowInsets"),
                    jni_sig!("()Landroid/view/WindowInsets;"),
                    &[],
                )?
                .l()?;
            if insets.is_null() {
                return Ok(None);
            }

            let sdk_int = env
                .get_static_field(
                    jni_str!("android/os/Build$VERSION"),
                    jni_str!("SDK_INT"),
                    jni_sig!("I"),
                )?
                .i()?;
            if sdk_int >= 30 {
                let system_bars = env
                    .call_static_method(
                        jni_str!("android/view/WindowInsets$Type"),
                        jni_str!("systemBars"),
                        jni_sig!("()I"),
                        &[],
                    )?
                    .i()?;
                let stable = env
                    .call_method(
                        &insets,
                        jni_str!("getInsetsIgnoringVisibility"),
                        jni_sig!("(I)Landroid/graphics/Insets;"),
                        &[JValue::Int(system_bars)],
                    )?
                    .l()?;
                if stable.is_null() {
                    return Ok(None);
                }
                return Ok(Some(AndroidSystemInsets {
                    left: env
                        .get_field(&stable, jni_str!("left"), jni_sig!("I"))?
                        .i()?,
                    top: env
                        .get_field(&stable, jni_str!("top"), jni_sig!("I"))?
                        .i()?,
                    right: env
                        .get_field(&stable, jni_str!("right"), jni_sig!("I"))?
                        .i()?,
                    bottom: env
                        .get_field(&stable, jni_str!("bottom"), jni_sig!("I"))?
                        .i()?,
                }));
            }

            let inset = |env: &mut jni::Env<'_>, name| -> jni::errors::Result<i32> {
                env.call_method(&insets, name, jni_sig!("()I"), &[])?.i()
            };
            Ok(Some(AndroidSystemInsets {
                left: inset(env, jni_str!("getSystemWindowInsetLeft"))?,
                top: inset(env, jni_str!("getSystemWindowInsetTop"))?,
                right: inset(env, jni_str!("getSystemWindowInsetRight"))?,
                bottom: inset(env, jni_str!("getSystemWindowInsetBottom"))?,
            }))
        })
        .map_err(|error| error.to_string())
    }

    fn make_raw_input(
        app: &AndroidApp,
        ctx: &egui::Context,
        input: &mut AndroidInput,
        focused: bool,
        size_in_pixels: [u32; 2],
        started: Instant,
        fullscreen: bool,
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
        let safe_area_insets = android_system_window_insets(app, fullscreen)
            .ok()
            .flatten()
            .map(|insets| {
                egui::SafeAreaInsets(egui::epaint::MarginF32 {
                    left: insets.left.max(0) as f32 / pixels_per_point,
                    top: insets.top.max(0) as f32 / pixels_per_point,
                    right: insets.right.max(0) as f32 / pixels_per_point,
                    bottom: insets.bottom.max(0) as f32 / pixels_per_point,
                })
            })
            .or_else(|| {
                let content = app.content_rect();
                (content.right > content.left && content.bottom > content.top).then(|| {
                    egui::SafeAreaInsets(egui::epaint::MarginF32 {
                        left: content.left.max(0) as f32 / pixels_per_point,
                        top: content.top.max(0) as f32 / pixels_per_point,
                        right: (size_in_pixels[0] as i32 - content.right).max(0) as f32
                            / pixels_per_point,
                        bottom: (size_in_pixels[1] as i32 - content.bottom).max(0) as f32
                            / pixels_per_point,
                    })
                })
            });

        let mut raw = RawInput {
            screen_rect: Some(screen_rect),
            safe_area_insets,
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

    #[allow(unsafe_code)]
    fn set_android_fullscreen(app: &AndroidApp, fullscreen: bool) -> Result<(), String> {
        use jni::{
            JavaVM, jni_sig, jni_str,
            objects::{JObject, JValue},
        };

        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity_raw = app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            let window = env
                .call_method(
                    &activity,
                    jni_str!("getWindow"),
                    jni_sig!("()Landroid/view/Window;"),
                    &[],
                )?
                .l()?;
            let sdk_int = env
                .get_static_field(
                    jni_str!("android/os/Build$VERSION"),
                    jni_str!("SDK_INT"),
                    jni_sig!("I"),
                )?
                .i()?;

            if sdk_int >= 30 {
                let controller = env
                    .call_method(
                        &window,
                        jni_str!("getInsetsController"),
                        jni_sig!("()Landroid/view/WindowInsetsController;"),
                        &[],
                    )?
                    .l()?;
                if !controller.is_null() {
                    const SYSTEM_BARS: i32 = 1 | 2;
                    const LIGHT_BARS: i32 = 8 | 16;
                    if fullscreen {
                        // WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE.
                        env.call_method(
                            &controller,
                            jni_str!("setSystemBarsBehavior"),
                            jni_sig!("(I)V"),
                            &[JValue::Int(2)],
                        )?;
                        // Transient bars shown over fullscreen video should use light
                        // foreground icons rather than inheriting normal mode's dark ones.
                        env.call_method(
                            &controller,
                            jni_str!("setSystemBarsAppearance"),
                            jni_sig!("(II)V"),
                            &[JValue::Int(0), JValue::Int(LIGHT_BARS)],
                        )?;
                        env.call_method(
                            &controller,
                            jni_str!("hide"),
                            jni_sig!("(I)V"),
                            &[JValue::Int(SYSTEM_BARS)],
                        )?;
                    } else {
                        // Restore normal light-system-bar appearance before making the
                        // bars visible again. Some Samsung builds otherwise keep
                        // SystemUI (including the notification shade) in fullscreen's
                        // light-foreground mode until a later appearance update.
                        env.call_method(
                            &controller,
                            jni_str!("setSystemBarsAppearance"),
                            jni_sig!("(II)V"),
                            &[JValue::Int(LIGHT_BARS), JValue::Int(LIGHT_BARS)],
                        )?;
                        // Fullscreen enables transient bars. Restore normal bar
                        // behaviour as part of the same transition back to windowed UI.
                        env.call_method(
                            &controller,
                            jni_str!("setSystemBarsBehavior"),
                            jni_sig!("(I)V"),
                            &[JValue::Int(1)],
                        )?;
                        env.call_method(
                            &controller,
                            jni_str!("show"),
                            jni_sig!("(I)V"),
                            &[JValue::Int(SYSTEM_BARS)],
                        )?;
                    }
                }
            } else {
                let decor = env
                    .call_method(
                        &window,
                        jni_str!("getDecorView"),
                        jni_sig!("()Landroid/view/View;"),
                        &[],
                    )?
                    .l()?;
                let flags = if fullscreen {
                    // IMMERSIVE_STICKY | FULLSCREEN | HIDE_NAVIGATION plus layout
                    // flags so the app can use the whole display while bars are hidden.
                    0x0000_1000
                        | 0x0000_0004
                        | 0x0000_0002
                        | 0x0000_0100
                        | 0x0000_0200
                        | 0x0000_0400
                } else {
                    let mut flags = 0x0000_2000; // LIGHT_STATUS_BAR
                    if sdk_int >= 26 {
                        flags |= 0x0000_0010; // LIGHT_NAVIGATION_BAR
                    }
                    flags
                };
                env.call_method(
                    &decor,
                    jni_str!("setSystemUiVisibility"),
                    jni_sig!("(I)V"),
                    &[JValue::Int(flags)],
                )?;
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
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

    fn apply_commands(
        app: &AndroidApp,
        state: &mut AppState,
        commands: Vec<AppCommand>,
        fullscreen: &mut bool,
        media_focus: &mut AndroidMediaFocus,
    ) {
        for command in commands {
            let starts_playback = match &command {
                AppCommand::Play => matches!(
                    state.playback_state(),
                    PlaybackState::Paused | PlaybackState::Seeking
                ),
                AppCommand::TogglePlayback => {
                    matches!(state.playback_state(), PlaybackState::Paused)
                }
                _ => false,
            };
            if starts_playback && !media_focus.ensure_focus(app) {
                continue;
            }

            if matches!(state.apply(command), Some(AppEffect::ToggleFullscreen)) {
                *fullscreen = !*fullscreen;
                match set_android_fullscreen(app, *fullscreen) {
                    Ok(()) => log::info!("SanctuaryPlayer: Android fullscreen={}", *fullscreen),
                    Err(error) => log::error!(
                        "SanctuaryPlayer: could not set Android fullscreen={}: {error}",
                        *fullscreen
                    ),
                }
            }
            media_focus.sync_playback_state(app, state);
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

        let decode_mode = match option_env!("SANCTUARY_ANDROID_DECODE_MODE") {
            Some(value) => value.parse::<DecodeMode>().unwrap_or_else(|error| {
                panic!("invalid SANCTUARY_ANDROID_DECODE_MODE={value:?}: {error}")
            }),
            None => DecodeMode::platform_default(),
        };
        log::info!("SanctuaryPlayer: Android decode mode = {decode_mode}");
        let mut state = AppState::with_decode_mode(decode_mode);
        state.set_settings_path(internal_data_path.join("settings.json"));
        state.set_session_path(internal_data_path.join("session.json"));
        let playback_waker = android_app.create_waker();
        state.set_playback_wake(PlaybackWake::new(move || {
            playback_waker.wake();
        }));
        // Decoder auto-selection needs the active Vulkan device capability
        // before choosing MediaCodec direct vs readback. Defer restored-session
        // playback until InitWindow has established that renderer contract.
        let mut startup_source = state.take_startup_session_source();
        let (media_event_tx, media_event_rx): (
            Sender<AndroidMediaEvent>,
            Receiver<AndroidMediaEvent>,
        ) = mpsc::channel();
        install_media_event_bridge(media_event_tx, &android_app);
        if let Err(error) = android_initialise_media_integration(&android_app) {
            log::error!("SanctuaryPlayer: could not initialise Android media integration: {error}");
        }
        let mut media_focus = AndroidMediaFocus::default();
        let mut screen_on = AndroidScreenOn::default();

        let started = Instant::now();
        let mut last_update = started;
        let mut gpu: Option<AndroidGpu> = None;
        let mut input = AndroidInput::default();
        let mut ime = AndroidIme::default();
        let mut focused = false;
        let mut input_available = false;
        let mut fullscreen = false;
        let mut running = true;

        while running {
            let now = Instant::now();
            let timeout = min_timeout(repaint.timeout(), state_timeout(&state, now));
            android_app.poll_events(timeout, |event| match event {
                PollEvent::Main(MainEvent::Destroy) => {
                    log::info!("SanctuaryPlayer: Android GameActivity destroyed");
                    state.flush_persistence_for_shutdown(SHUTDOWN_POSITION_FLUSH_BUDGET);
                    media_focus.abandon(&android_app);
                    screen_on.set_foreground(&android_app, &state, false);
                    if let Err(error) = android_shutdown_media_integration(&android_app) {
                        log::error!(
                            "SanctuaryPlayer: could not shut down Android media integration: {error}"
                        );
                    }
                    uninstall_media_event_bridge();
                    oxideav_mediacodec::set_direct_presentation_available(false);
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
                        oxideav_mediacodec::set_direct_presentation_available(false);
                        log::error!(
                            "SanctuaryPlayer: could not attach Android render surface: {error}"
                        );
                    } else if let Some(source) = startup_source.take() {
                        let _ = state.apply(AppCommand::OpenVideo(source));
                    }
                    if let Err(error) = set_android_fullscreen(&android_app, fullscreen) {
                        log::error!(
                            "SanctuaryPlayer: could not apply Android system-bar mode fullscreen={fullscreen}: {error}"
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
                    if let Err(error) = set_android_fullscreen(&android_app, fullscreen) {
                        log::error!(
                            "SanctuaryPlayer: could not restore Android system-bar mode fullscreen={fullscreen}: {error}"
                        );
                    }
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::LostFocus) => {
                    focused = false;
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::Pause | MainEvent::Stop) => {
                    state.pause_for_background();
                    media_focus.abandon(&android_app);
                    screen_on.set_foreground(&android_app, &state, false);
                }
                PollEvent::Main(MainEvent::SaveState { .. }) => {
                    state.flush_persistence_for_background();
                }
                PollEvent::Main(MainEvent::Resume { .. }) => {
                    screen_on.set_foreground(&android_app, &state, true);
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Main(MainEvent::RedrawNeeded { .. })
                | PollEvent::Main(MainEvent::ContentRectChanged { .. })
                | PollEvent::Main(MainEvent::InsetsChanged { .. })
                | PollEvent::Main(MainEvent::ConfigChanged { .. })
                | PollEvent::Main(MainEvent::Start) => {
                    repaint.request(Duration::ZERO);
                }
                PollEvent::Wake => {
                    let mut media_event_handled = false;
                    while let Ok(event) = media_event_rx.try_recv() {
                        media_focus.handle_event(&android_app, &mut state, event);
                        media_event_handled = true;
                    }
                    screen_on.sync(&android_app, &state);
                    let playback_wakes = state.take_playback_wakes();
                    if media_event_handled || !playback_wakes.is_empty() {
                        repaint.request(Duration::ZERO);
                    }
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
                media_focus.sync_playback_state(&android_app, &mut state);
                screen_on.sync(&android_app, &state);
                last_update = now;
                let user_tapped = std::mem::take(&mut input.primary_pointer_pressed);
                let raw_input = make_raw_input(
                    &android_app,
                    &ctx,
                    &mut input,
                    focused,
                    size,
                    started,
                    fullscreen,
                );
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
                            apply_commands(
                                &android_app,
                                &mut state,
                                commands,
                                &mut fullscreen,
                                &mut media_focus,
                            );
                            screen_on.sync(&android_app, &state);
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
