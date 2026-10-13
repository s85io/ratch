#![forbid(unsafe_code)]

fn main() {
    // A panic aborts, so no destructor will put the user's terminal back.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratch::sys::restore_remembered_terminal();
        default_hook(info);
    }));
    std::process::exit(ratch::cli::run(std::env::args_os().collect()));
}
