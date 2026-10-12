use std::process::ExitCode;

use anyhow::Result;

use super::Session;

pub(crate) fn run(
    _session: &Session,
    _query: String,
    _tmux: bool,
    _json: bool,
    _command: Vec<String>,
) -> Result<ExitCode> {
    anyhow::bail!("pick is wired up in the pick step")
}
