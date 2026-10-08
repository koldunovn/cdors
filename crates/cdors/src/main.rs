fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("cdors {}", cdors_core::VERSION);
            println!("{}", cdors_core::native_library_versions());
        }
        _ => {
            eprintln!(
                "cdors {}: no operators implemented yet (try --version)",
                cdors_core::VERSION
            );
            std::process::exit(2);
        }
    }
}
