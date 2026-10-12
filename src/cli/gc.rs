use std::process::ExitCode;

use anyhow::Result;

use super::Session;

pub(crate) fn run(_session: &Session) -> Result<ExitCode> {
    anyhow::bail!("gc is wired up in the UI step")
}
