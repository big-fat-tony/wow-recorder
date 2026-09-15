// No console window on Windows; logs go to a file (see lib.rs).
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    wow_recorder_lib::run()
}
