#[tokio::main]
async fn main() {
    std::process::exit(antiphon::run_cli().await);
}
