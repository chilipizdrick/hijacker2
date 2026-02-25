use clap::Parser;
use hijacker2::cli_args::Args;

fn main() {
    let args = Args::parse();

    env_logger::init_from_env(env_logger::Env::default());

    pipewire::init();

    if let Err(err) = hijacker2::run(args) {
        eprintln!("Error: {err}");
        if let Some(source) = err.source() {
            eprintln!("Caused by: {source}");
        }
        std::process::exit(1);
    }

    unsafe { pipewire::deinit() };
}
