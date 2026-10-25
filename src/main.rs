#![cfg_attr(windows, windows_subsystem = "windows")]

mod api;
mod config;
mod platform;
mod timefmt;

fn main() {
    platform::run();
}
