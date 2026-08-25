mod commands;
mod dispatch;
mod state;
mod ui;
mod watch;

use std::env;
use std::process;

use state::State;

fn main() {
    let state = State::discover();
    let args: Vec<String> = env::args().skip(1).collect();
    match commands::run(&state, &args) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("{error:#}");
            process::exit(1);
        }
    }
}
