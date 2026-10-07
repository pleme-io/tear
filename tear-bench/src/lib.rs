#![deny(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

pub mod floor;
pub mod gate;
pub mod harness;
pub mod matrix;
pub mod receipt;
pub mod seam;
pub mod stats;
mod table;
pub mod verdict;
