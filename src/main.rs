mod app;
mod assets;
mod history;
mod model;
mod telemetry;
mod update;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("OYAsys: {error}");
        std::process::exit(1);
    }
}
