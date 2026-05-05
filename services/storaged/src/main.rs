use std::thread;
use std::time::Duration;

fn main() {
    observability_client::info!("storaged", service, "service started");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
