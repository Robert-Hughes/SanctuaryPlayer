//! Android packaging/entry-point wrapper for the shared SanctuaryPlayer app.

#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    let settings_path = android_app
        .internal_data_path()
        .map(|path| path.join("settings.json"));
    let event_loop =
        winit::event_loop::EventLoop::<sanctuary_player_app::AppEvent>::with_user_event()
            .with_android_app(android_app)
            .build()
            .expect("failed to create Android event loop");
    let mut app = sanctuary_player_app::SanctuaryPlayerApp::new();
    app.set_event_proxy(event_loop.create_proxy());
    if let Some(path) = settings_path {
        app.set_settings_path(path);
    } else {
        eprintln!(
            "SanctuaryPlayer: Android internal data path is unavailable; settings will not persist"
        );
    }
    event_loop
        .run_app(&mut app)
        .expect("SanctuaryPlayer event loop failed");
}
