use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use flate2::{Compression, write::GzEncoder};
use goblin::{Object, elf, mach};
use sha2::{Digest, Sha256};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a portable release archive and its SHA-256 checksum.
    Package {
        binary: PathBuf,
        #[arg(value_enum)]
        target: Target,
        #[arg(long, default_value = "dist")]
        output: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    #[value(name = "x86_64-unknown-linux-musl")]
    LinuxX86_64,
    #[value(name = "aarch64-unknown-linux-musl")]
    LinuxAarch64,
    #[value(name = "aarch64-apple-darwin")]
    MacosAarch64,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Self::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Self::MacosAarch64 => "aarch64-apple-darwin",
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Package {
            binary,
            target,
            output,
        } => package(&binary, target, &output),
    }
}

fn validate_binary(binary: &[u8], target: Target) -> Result<()> {
    match (target, Object::parse(binary)?) {
        (Target::LinuxX86_64, Object::Elf(binary)) => {
            validate_elf(&binary, elf::header::EM_X86_64)?;
        }
        (Target::LinuxAarch64, Object::Elf(binary)) => {
            validate_elf(&binary, elf::header::EM_AARCH64)?;
        }
        (Target::MacosAarch64, Object::Mach(mach::Mach::Binary(binary))) => {
            ensure!(
                binary.header.cputype == mach::constants::cputype::CPU_TYPE_ARM64,
                "wrong CPU architecture"
            );
            for library in binary.libs {
                ensure!(
                    !["/nix/store", "/opt/homebrew", "/usr/local"]
                        .iter()
                        .any(|prefix| library.starts_with(prefix)),
                    "binary depends on build-machine library: {library}"
                );
            }
        }
        _ => bail!("binary format does not match {}", target.name()),
    }
    Ok(())
}

fn validate_elf(binary: &elf::Elf<'_>, machine: u16) -> Result<()> {
    ensure!(binary.header.e_machine == machine, "wrong CPU architecture");
    ensure!(
        binary.interpreter.is_none() && binary.libraries.is_empty(),
        "Linux release binary must be statically linked"
    );
    Ok(())
}

fn package(binary: &Path, target: Target, output: &Path) -> Result<()> {
    let bytes = fs::read(binary).with_context(|| format!("read {}", binary.display()))?;
    validate_binary(&bytes, target).context("validate release binary")?;

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask must live in the workspace")?;
    fs::create_dir_all(output)?;
    let filename = format!("git-yard-{}.tar.gz", target.name());
    let archive_path = output.join(&filename);
    let mut archive = tar::Builder::new(GzEncoder::new(
        File::create(&archive_path)?,
        Compression::default(),
    ));
    for (path, name, mode) in [
        (binary.to_path_buf(), "git-yard", 0o755),
        (root.join("LICENSE"), "LICENSE", 0o644),
        (root.join("README.md"), "README.md", 0o644),
    ] {
        let mut file = File::open(&path)?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&file.metadata()?);
        header.set_mode(mode);
        header.set_cksum();
        archive.append_data(&mut header, name, &mut file)?;
    }
    archive.into_inner()?.finish()?;

    let checksum = hex::encode(Sha256::digest(fs::read(&archive_path)?));
    fs::write(
        output.join(format!("{filename}.sha256")),
        format!("{checksum}  {filename}\n"),
    )?;
    println!("{}", archive_path.display());
    Ok(())
}
