//! Android packaging/entry-point wrapper for the shared SanctuaryPlayer app.

#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    let internal_data_path = android_app
        .internal_data_path()
        .expect("Android internal data path is unavailable");
    sanctuary_player_app::logging::init(internal_data_path.join("logs"))
        .expect("failed to initialise Android file logging");
    let settings_path = internal_data_path.join("settings.json");
    let session_path = internal_data_path.join("session.json");
    let event_loop =
        match winit::event_loop::EventLoop::<sanctuary_player_app::AppEvent>::with_user_event()
            .with_android_app(android_app)
            .build()
        {
            Ok(event_loop) => event_loop,
            Err(error) => {
                log::error!("SanctuaryPlayer: failed to create Android event loop: {error}");
                log::logger().flush();
                return;
            }
        };
    let mut app = sanctuary_player_app::SanctuaryPlayerApp::new();
    app.set_event_proxy(event_loop.create_proxy());
    app.set_settings_path(settings_path);
    app.set_session_path(session_path);
    if let Err(error) = event_loop.run_app(&mut app) {
        log::error!("SanctuaryPlayer: Android event loop failed: {error}");
        app.flush_persistence_for_shutdown();
        log::logger().flush();
    }
}
