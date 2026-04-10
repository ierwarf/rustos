use std::thread;
use std::time::Duration;

fn main() {
    diag_client::diag_info!("inputd", "service started");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
