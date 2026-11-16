use crate::proto::{
    FLAG_KEYBOARD_READY, FLAG_POINTING_READY, FLAG_VERSION_MISMATCH, KeyState,
};

unsafe extern "C" {
    fn batonpass_vhid_start(vendor_id: u16, product_id: u16, country_code: u8);
    fn batonpass_vhid_stop();
    fn batonpass_vhid_flags() -> u32;
    fn batonpass_vhid_keyboard(modifiers: u8, keys: *const u16, count: usize);
    fn batonpass_vhid_consumer(keys: *const u16, count: usize);
    fn batonpass_vhid_pointing(buttons: u32, x: i8, y: i8, vertical_wheel: i8, horizontal_wheel: i8);
}

const APPLE_VENDOR: u16 = 0x05ac;
const ANSI_KEYBOARD: u16 = 0x024f;

/// Connects to Karabiner's virtual HID daemon, posing as the same ANSI keyboard Karabiner-Elements
/// uses so macOS never shows its keyboard setup assistant.
pub fn start() {
    unsafe { batonpass_vhid_start(APPLE_VENDOR, ANSI_KEYBOARD, 0) }
}

/// Disconnects from Karabiner's daemon and joins the client's threads. Run before the process
/// exits: left running, they lock mutexes that exit-time destructors have already torn down,
/// and the process aborts instead of exiting.
pub fn stop() {
    unsafe { batonpass_vhid_stop() }
}

pub fn flags() -> u8 {
    let raw = unsafe { batonpass_vhid_flags() };
    let mut flags = 0;
    if raw & (1 << 3) != 0 {
        flags |= FLAG_VERSION_MISMATCH;
    }
    if raw & (1 << 4) != 0 {
        flags |= FLAG_KEYBOARD_READY;
    }
    if raw & (1 << 5) != 0 {
        flags |= FLAG_POINTING_READY;
    }
    flags
}

pub fn pointing_ready() -> bool {
    flags() & FLAG_POINTING_READY != 0
}

pub fn keyboard(state: &KeyState) {
    unsafe { batonpass_vhid_keyboard(state.modifiers, state.keys.as_ptr(), state.keys.len()) }
}

pub fn consumer(keys: &[u16]) {
    unsafe { batonpass_vhid_consumer(keys.as_ptr(), keys.len()) }
}

pub fn pointing(buttons: u32, x: i8, y: i8, vertical_wheel: i8, horizontal_wheel: i8) {
    unsafe { batonpass_vhid_pointing(buttons, x, y, vertical_wheel, horizontal_wheel) }
}
