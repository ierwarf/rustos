use std::thread;
use std::time::Duration;

fn main() {
    eprintln!("inputd: service started");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
