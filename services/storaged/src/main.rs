use std::thread;
use std::time::Duration;

fn main() {
    eprintln!("storaged: service started");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
