use anyhow::Result;
use clap::Args;
use clap_complete::Shell;

#[derive(Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: Shell,
}

pub fn run(args: CompletionsArgs) -> Result<()> {
    let content = generate_to_string(args.shell);
    print!("{content}");
    Ok(())
}

/// Generate completions for the given shell and return as a String.
pub fn generate_to_string(shell: Shell) -> String {
    use clap::CommandFactory;
    use clap_complete::generate;

    let mut cmd = super::Cli::command();
    let bin_name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    generate(shell, &mut cmd, bin_name, &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}
