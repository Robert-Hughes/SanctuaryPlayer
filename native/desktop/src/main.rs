fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = winit::event_loop::EventLoop::new()?;
    let mut app = sanctuary_player_app::SanctuaryPlayerApp::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}
