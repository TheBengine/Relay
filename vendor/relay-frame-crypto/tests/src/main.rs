extern "C" {
    fn relay_owned_iv_probe() -> i32;
}
fn main() {
    // Test-only FFI: synthetic frames, no media capture, accounts or network.
    std::process::exit(unsafe { relay_owned_iv_probe() });
}
