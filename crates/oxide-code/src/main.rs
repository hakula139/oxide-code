use clap::Parser;

#[derive(Parser)]
#[command(
    name = "oxide-code",
    version,
    about = "A terminal-based AI coding assistant"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!("oxide-code");
}
