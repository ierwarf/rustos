// Protocol projection of complete normal-sink ownership, not port I/O or panic.
fn user_exception_marker_frame_race() {
    let stream = Arc::new(Mutex::new(Vec::new()));
    let frame_stream = stream.clone();
    let frame = thread::spawn(move || {
        frame_stream.lock().unwrap().extend_from_slice(b"FRAME\n");
    });
    let marker_stream = stream.clone();
    let marker = thread::spawn(move || {
        marker_stream.lock().unwrap().extend_from_slice(b"!EX:0e\n");
    });
    frame.join().unwrap();
    marker.join().unwrap();
    let stream = stream.lock().unwrap();
    assert!(stream.as_slice() == b"FRAME\n!EX:0e\n" || stream.as_slice() == b"!EX:0e\nFRAME\n");
}

#[test]
fn user_exception_marker_cannot_split_a_structured_frame() {
    loom::model(user_exception_marker_frame_race);
}
