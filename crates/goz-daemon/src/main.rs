//! gozd: the goz indexing daemon.
//!
//! Bootstraps per-volume indexes from the NTFS MFT, tails the USN change
//! journal, and serves queries over `\\.\pipe\goz-v1`. `run` starts the live
//! daemon (console by default; `--service` under the SCM); `scan` does a
//! one-shot bootstrap and prints stats; `install`/`uninstall` register the
//! auto-start Windows service.

// Thread-caching allocator: the query path allocates a path buffer per hit, so a
// broad query is millions of small allocations that the Windows default heap
// serializes. See the workspace Cargo.toml note.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(windows)]
mod bootstrap;
#[cfg(windows)]
mod pulse;
#[cfg(windows)]
mod run;
#[cfg(windows)]
mod server;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod supervisor;
#[cfg(windows)]
mod tail;
#[cfg(windows)]
mod volume_state;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "gozd", version, about = "goz indexing daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap every fixed NTFS volume once and print index stats, then exit
    /// (no daemon). Pass a query to search the freshly-built index end-to-end.
    Scan {
        /// Optional query to run against each volume after bootstrap.
        query: Option<String>,
    },
    /// Run the daemon (console mode by default).
    Run {
        /// Run attached to the console with Ctrl+C handling (the default).
        #[arg(long)]
        console: bool,
        /// Run under the Windows Service Control Manager. Set automatically in
        /// the service's registered command line; not for interactive use.
        #[arg(long)]
        service: bool,
    },
    /// Install and start the auto-start Windows service (requires elevation).
    Install,
    /// Stop and remove the Windows service (requires elevation).
    Uninstall,
    /// Report whether the Windows service is installed and running.
    Status,
}

/// Configure mimalloc for a long-lived cache-like process: purge freed
/// memory back to the OS immediately instead of after a delayed tick.
///
/// mimalloc's default purge runs `purge_delay` ms after a free, but only on
/// later allocator activity; an idle daemon never ticks, so the freed
/// bootstrap transients (sparse FRN maps, enumeration buffers) stayed
/// COMMITTED for the daemon's lifetime (~100 MB against a ~490 MB process).
///
/// The option indexes are the bundled mimalloc v2 enum, which the sys crate
/// does not export. Each is verified by reading back its documented default
/// before writing: if the layout ever shifts, the mismatch skips the write
/// instead of flipping an arbitrary option.
#[cfg(windows)]
fn tune_allocator() {
    /// v2 `mi_option_abandoned_page_purge` (default 0).
    const ABANDONED_PAGE_PURGE: i32 = 12;
    /// v2 `mi_option_purge_delay` in ms (default 10).
    const PURGE_DELAY: i32 = 15;
    // SAFETY: mi_option_get/set take no pointers and are safe at any time.
    unsafe {
        if libmimalloc_sys::mi_option_get(PURGE_DELAY) == 10 {
            libmimalloc_sys::mi_option_set(PURGE_DELAY, 0);
        }
        if libmimalloc_sys::mi_option_get(ABANDONED_PAGE_PURGE) == 0 {
            libmimalloc_sys::mi_option_set(ABANDONED_PAGE_PURGE, 1);
        }
    }
}

/// Off Windows there is no mimalloc global allocator to tune (the daemon only
/// runs on Windows anyway), so this is a no-op that keeps `main` platform-free.
#[cfg(not(windows))]
fn tune_allocator() {}

fn main() -> anyhow::Result<()> {
    tune_allocator();
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { query } => run_scan(query),
        Command::Run { service, .. } => run_daemon(service),
        Command::Install => do_install(),
        Command::Uninstall => do_uninstall(),
        Command::Status => do_status(),
    }
}

#[cfg(windows)]
fn run_daemon(service: bool) -> anyhow::Result<()> {
    if service {
        service::run()
    } else {
        run::run_console()
    }
}

#[cfg(not(windows))]
fn run_daemon(_service: bool) -> anyhow::Result<()> {
    anyhow::bail!("goz requires Windows (it reads the NTFS MFT and USN journal)")
}

#[cfg(windows)]
fn do_install() -> anyhow::Result<()> {
    service::install()
}

#[cfg(windows)]
fn do_uninstall() -> anyhow::Result<()> {
    service::uninstall()
}

#[cfg(windows)]
fn do_status() -> anyhow::Result<()> {
    service::status()
}

#[cfg(not(windows))]
fn do_install() -> anyhow::Result<()> {
    anyhow::bail!("the goz service requires Windows")
}

#[cfg(not(windows))]
fn do_uninstall() -> anyhow::Result<()> {
    anyhow::bail!("the goz service requires Windows")
}

#[cfg(not(windows))]
fn do_status() -> anyhow::Result<()> {
    anyhow::bail!("the goz service requires Windows")
}

#[cfg(windows)]
fn run_scan(query: Option<String>) -> anyhow::Result<()> {
    use std::time::Instant;

    if !goz_winfs::is_elevated() {
        eprintln!("note: gozd is not elevated; raw volume access will likely fail.");
        eprintln!("      run from an elevated terminal for a real scan.\n");
    }

    let started = Instant::now();
    let scans = bootstrap::scan_all()?.ok;
    let total_entries: usize = scans
        .iter()
        .map(bootstrap::BootstrappedVolume::entries)
        .sum();

    for s in &scans {
        let mount = s
            .mounts
            .first()
            .cloned()
            .unwrap_or_else(|| s.guid_path.clone());
        println!("volume {mount}");
        println!("  guid          {}", s.guid_path);
        println!("  entries       {}", s.entries());
        println!("  enum          {:.2}s", s.enum_secs);
        println!("  file_layout   {:.2}s", s.layout_secs);
        match &s.cursor {
            Some(c) => println!(
                "  journal       id={:#x} next_usn={}",
                c.journal_id, c.next_usn
            ),
            None => println!("  journal       (none: live updates unavailable)"),
        }
    }
    println!(
        "\nindexed {} entries across {} volume(s) in {:.2}s",
        total_entries,
        scans.len(),
        started.elapsed().as_secs_f64()
    );

    if let Some(q) = query {
        run_demo_query(&scans, &q)?;
    }
    Ok(())
}

/// Runs a query against each volume's freshly-built index and prints the first
/// results: the end-to-end smoke test (bootstrap → search).
#[cfg(windows)]
fn run_demo_query(scans: &[bootstrap::BootstrappedVolume], query: &str) -> anyhow::Result<()> {
    use goz_core::query::{parse_query, run_query};
    use goz_core::types::SortSpec;
    use std::time::Instant;

    let parsed = parse_query(query).map_err(|e| anyhow::anyhow!("bad query: {e}"))?;
    println!("\nquery: {query:?}");
    for s in scans {
        let mount = s.mounts.first().cloned().unwrap_or_default();
        let t = Instant::now();
        let out = run_query(&s.index, &parsed, None, SortSpec::default(), 0, Some(20));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  {}: {} match(es) in {:.1} ms (showing up to 20)",
            s.mounts.first().map(String::as_str).unwrap_or(&s.guid_path),
            out.total,
            ms
        );
        for hit in &out.hits {
            let path = goz_core::wtf8::to_string_lossy(&hit.path);
            let size = match hit.size {
                Some(b) => format!("{b} B"),
                None => "?".to_string(),
            };
            let date = match hit.mtime {
                Some(ft) => {
                    let ms = goz_core::output::filetime_to_unix_ms(ft);
                    format!("{ms} ms")
                }
                None => "?".to_string(),
            };
            println!("    [{size:>12}] [{date:>15}] {mount}{path}");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_scan(_query: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("goz requires Windows (it reads the NTFS MFT and USN journal)")
}
