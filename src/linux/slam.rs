use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use std::time::Duration;

/// A private virtual mouse that shoves the cursor into the capture barrier, which is how the
/// hotkey starts a capture: the portal only activates when the pointer crosses a barrier.
pub struct Slammer(VirtualDevice);

impl Slammer {
    pub fn new() -> std::io::Result<Self> {
        let device = VirtualDevice::builder()?
            .name("ferry hotkey pointer")
            .with_relative_axes(&AttributeSet::from_iter([RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y]))?
            .with_keys(&AttributeSet::from_iter([KeyCode::BTN_LEFT]))?
            .build()?;
        Ok(Self(device))
    }

    /// Moves along `perpendicular` first when the cursor is outside the barrier's span, then
    /// pushes against the barrier in `toward` steps so the compositor sees a crossing attempt.
    pub async fn slam(&mut self, toward: (i32, i32), perpendicular: Option<(i32, i32)>) -> std::io::Result<()> {
        if let Some((x, y)) = perpendicular {
            self.motion(x * 30_000, y * 30_000)?;
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
        self.motion(toward.0 * 30_000, toward.1 * 30_000)?;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(8)).await;
            self.motion(toward.0 * 40, toward.1 * 40)?;
        }
        Ok(())
    }

    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        let relative = EventType::RELATIVE.0;
        self.0.emit(&[
            InputEvent::new(relative, RelativeAxisCode::REL_X.0, dx),
            InputEvent::new(relative, RelativeAxisCode::REL_Y.0, dy),
        ])
    }
}
