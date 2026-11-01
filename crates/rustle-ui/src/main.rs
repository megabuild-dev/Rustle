mod api;
mod app;
mod common;
mod cron;
mod overview;
mod settings;
mod sites;
mod wizard;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
