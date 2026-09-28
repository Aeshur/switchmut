#![windows_subsystem = "windows"]
use std::process::ExitCode;

fn main() -> ExitCode {
    match switchmut::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            switchmut::ui::message(std::ptr::null_mut(), &error);
            ExitCode::from(1)
        }
    }
}
