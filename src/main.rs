use std::process::ExitCode;

// musl's allocator serialises heavily under the worker threads; mimalloc
// keeps the static build as fast as the glibc one.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    git_yard::cli::run()
}
