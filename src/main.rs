fn main() {
    let result = std::thread::Builder::new()
        .name("ads-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(ads::run)
        .expect("failed to start ads main thread")
        .join()
        .expect("ads main thread panicked");

    if let Err(error) = result {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}
