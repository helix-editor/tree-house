use std::path::{Path, PathBuf};
use std::process::exit;

use ::skidder::list_grammars;
use anyhow::Result;

mod build;
mod flags;
mod generate_parser;
mod import;
mod init;
mod load;

fn get_version() -> String {
    const GIT_HASH: Option<&str> = option_env!("GIT_HASH");
    const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");

    let owned = CARGO_VERSION.to_string();
    if let Some(hash) = GIT_HASH {
        owned + "-" + hash
    } else {
        owned
    }
}

fn wrapped_main() -> Result<()> {
    let flags = flags::Skidder::from_env_or_exit();
    match flags.subcommand {
        flags::SkidderCmd::Import(import_cmd) => import_cmd.run(),
        flags::SkidderCmd::Build(build_cmd) => build_cmd.run(),
        flags::SkidderCmd::InitRepo(init_cmd) => init_cmd.run(),
        flags::SkidderCmd::LoadGrammar(load_cmd) => load_cmd.run(),
        flags::SkidderCmd::RegenerateParser(generate_cmd) => generate_cmd.run(),
        flags::SkidderCmd::Version(flags::Version { version }) => {
            if version {
                println!("skidder-cli {}", get_version());
            } else {
                println!("{}", flags::Skidder::HELP);
            }
            Ok(())
        }
    }
}

pub fn main() {
    if let Err(err) = wrapped_main() {
        for error in err.chain() {
            eprintln!("error: {error}")
        }
        exit(1)
    }
}

fn collect_grammars(repo: &Path) -> Result<Vec<PathBuf>> {
    let config = skidder::Config {
        repos: vec![skidder::Repo::Local {
            path: repo.to_owned(),
        }],
        index: PathBuf::new(),
        verbose: false,
    };
    list_grammars(&config)
}
