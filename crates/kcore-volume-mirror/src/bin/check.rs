fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Err(err) = kcore_volume_mirror::rehearse::run(&args) {
        eprintln!("local disk migration rehearsal failed: {err}");
        std::process::exit(1);
    }
}
