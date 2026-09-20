//! Prints the keycodes the app would post for ⌘C / ⌘V / ⌘Z in the current
//! keyboard layout, and whether Secure Input is on. Runs the real Text Input
//! Source lookup on the main thread; touches no user data and posts no events.
//!
//!     cargo run --example key_layout_probe
//!
//! On a QWERTY layout it prints `c=8 v=9 z=6`. Switch to Dvorak, AZERTY or
//! QWERTZ in System Settings and run it again to see the codes move.

fn main() {
    match fast_dictate_backend::key_layout::refresh() {
        Some((c, v, z)) => println!("layout lookup ok · c={c} v={v} z={z}"),
        None => println!("layout lookup unavailable · using ANSI fallbacks"),
    }
    println!(
        "posting · copy={} paste={} undo={}",
        fast_dictate_backend::key_layout::copy_key(),
        fast_dictate_backend::key_layout::paste_key(),
        fast_dictate_backend::key_layout::undo_key(),
    );
    println!("secure input · {}", fast_dictate_backend::secure_input::query());
}
