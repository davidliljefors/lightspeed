mod cli;

// The system heap takes a lock on most allocations; mimalloc is per-thread.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    std::process::exit(cli::run(std::env::args().skip(1).collect()));
}
