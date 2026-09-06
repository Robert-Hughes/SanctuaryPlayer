//! Android packaging/entry-point wrapper for the shared SanctuaryPlayer app.

#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    let event_loop = winit::event_loop::EventLoop::builder()
        .with_android_app(android_app)
        .build()
        .expect("failed to create Android event loop");
    let mut app = sanctuary_player_app::SanctuaryPlayerApp::new();
    event_loop
        .run_app(&mut app)
        .expect("SanctuaryPlayer event loop failed");
}
