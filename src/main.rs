use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    ExitCode::from(igdl::cli::run().await)
}
