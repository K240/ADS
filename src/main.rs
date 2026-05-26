fn main() {
    if let Err(error) = ads::run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}
