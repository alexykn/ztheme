mod cache;
mod cli;
mod daemon;
mod environment;
mod gitstatus;
mod prompt;
mod runtime;
mod setup;
mod theme;
mod utils;

fn main() {
    cli::run();
}
