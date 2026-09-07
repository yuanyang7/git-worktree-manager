fn main() {
    if let Err(error) = worktree_manager::run(std::env::args_os().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
