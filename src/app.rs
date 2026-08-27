pub(crate) mod build;
mod push;

use anyhow::Result;

use crate::auth::Credentials;
use crate::cli::{Command, parse};
use crate::progress::Progress;

pub async fn run() -> Result<()> {
    let cli = parse();
    let progress = Progress::new(cli.global.progress);
    crate::init_tracing(&progress);
    let credentials =
        Credentials::load(&cli.global.credential, cli.global.docker_config.as_deref())?;

    match cli.command {
        Command::Build(arguments) => {
            let specification = build::ImageBuildSpec::try_from(*arguments)?;
            build::BuildRequest::try_from(specification)?
                .run(credentials, progress)
                .await
        }
    }
}
