//! Standalone test binary for Vortex Bluetooth HID (UHID) Universal Control without ADB.
//!
//! Usage: cargo run --features dev-tools --bin vortex-bt-hid-test

use vortex_l3_daemon::core::bt_hid::BtHidServer;
use zbus::connection::Connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    println!("=====================================================");
    println!(" 🌀 Vortex Bluetooth HID (UHID) Universal Control Test ");
    println!("=====================================================");
    println!("1. Registering BlueZ HID Profile (Mouse + Keyboard)...");

    let sys_conn = Connection::system().await?;
    let hid_server = BtHidServer::new();

    if let Err(e) = hid_server.register(&sys_conn).await {
        println!("❌ Failed to register BlueZ HID Profile: {e}");
        println!("   (Make sure bluetoothd service is running)");
        return Err(e);
    }

    println!("✅ BlueZ HID Profile registered successfully!");
    println!("-----------------------------------------------------");
    println!("📱 INSTRUCTIONS:");
    println!("1. Open Bluetooth Settings on your Android Phone.");
    println!("2. Pair with this Linux Laptop.");
    println!("3. Your phone will recognize this laptop as an Input Device (Keyboard/Mouse).");
    println!("4. Once connected, move your mouse or press keys to test!");
    println!("-----------------------------------------------------");
    println!("Press Ctrl+C to stop.");

    // Keep process alive while listening for BlueZ HID connections
    tokio::signal::ctrl_c().await?;
    println!("\nShutting down Bluetooth HID test...");
    Ok(())
}
