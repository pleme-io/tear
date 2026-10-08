#![forbid(unsafe_code)]

mod args;
mod authority;
mod held;
mod holder;
pub mod proto;
pub mod sink;

pub use args::HoldArgs;
pub use authority::Authority;
pub use held::{HeldPty, HoldProgram, OnBytes, OnExit, Revival};
pub use holder::run;

pub fn main_from_args(argv: &[String]) -> ! {
    #[cfg(feature = "bench-probes")]
    makimono::probes::arm_from_env();
    match HoldArgs::parse(argv).and_then(run) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("tamotsu: {e:#}");
            std::process::exit(2)
        }
    }
}
