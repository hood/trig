use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use trig_core::{Case, Engine, Opts};

#[derive(Parser)]
#[command(name = "trig", about = "trigram code search index")]
struct Cli {
    /// Project root (defaults to cwd)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// RocksDB directory (defaults to $XDG_CACHE_HOME/nvim/trig/<hash>)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or refresh the index (incremental after the first run)
    Index {
        #[arg(long)]
        no_compact: bool,
    },
    /// Search, printing path:line:col:text
    Search {
        pattern: String,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        #[arg(short = 's', long)]
        case_sensitive: bool,
        #[arg(short = 'F', long)]
        fixed: bool,
        #[arg(long, default_value_t = 10_000)]
        max: usize,
        /// ANSI colors like rg --color=always (for fzf --ansi)
        #[arg(long)]
        color: bool,
        /// Print plan and timings to stderr
        #[arg(long)]
        stats: bool,
    },
    /// Re-index one file
    Update { path: PathBuf },
    /// Fold merge operands and purge tombstoned ids
    Compact,
    Stats,
    /// Print the path of libtrig (the C ABI lib) shipped next to this binary
    LibPath,
}

fn lib_path() -> Result<PathBuf> {
    let name = if cfg!(target_os = "macos") { "libtrig.dylib" } else { "libtrig.so" };
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().unwrap();
    [dir.join(name), dir.join("../lib").join(name)]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("{name} not found next to {}", exe.display()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.cmd, Cmd::LibPath) {
        println!("{}", lib_path()?.canonicalize()?.display());
        return Ok(());
    }
    let root = cli.root.unwrap_or(std::env::current_dir()?);
    let read_only = matches!(cli.cmd, Cmd::Search { .. } | Cmd::Stats);
    let t = Instant::now();
    let e = Engine::open(&root, cli.db.as_deref(), read_only)?;
    let open_us = t.elapsed().as_micros();
    match cli.cmd {
        Cmd::Index { no_compact } => {
            let st = e.reconcile(!no_compact)?;
            eprintln!("{st:?}\ndb: {}", e.db_path.display());
        }
        Cmd::Search { pattern, ignore_case, case_sensitive, fixed, max, color, stats } => {
            let case = if ignore_case {
                Case::Insensitive
            } else if case_sensitive {
                Case::Sensitive
            } else {
                Case::Smart
            };
            let (hits, st) = e.search(&pattern, &Opts { case, fixed, max })?;
            let mut out = String::with_capacity(hits.len() * 80);
            for h in &hits {
                if color { h.vimgrep_color(&mut out) } else { h.vimgrep(&mut out) }
            }
            let mut so = std::io::stdout().lock();
            let _ = so.write_all(out.as_bytes());
            if stats {
                eprintln!(
                    "plan={} candidates={} matched_files={} hits={} open={}us plan+fetch={}us verify={}us truncated={}",
                    st.plan, st.candidates, st.matched_files, hits.len(), open_us, st.plan_us, st.verify_us, st.truncated
                );
            }
        }
        Cmd::Update { path } => {
            e.update(&path)?;
        }
        Cmd::Compact => e.store.compact()?,
        Cmd::LibPath => unreachable!(),
        Cmd::Stats => {
            let mut docs = 0;
            e.store.for_each_doc(|_, _| docs += 1)?;
            println!("db: {}", e.db_path.display());
            println!("docs: {docs}  tombstones: {}  next_id: {}", e.store.tomb.read().len(), e.store.next_id.load(std::sync::atomic::Ordering::SeqCst));
            for cf in [trig_core::store::CF_POSTINGS, trig_core::store::CF_DOCS] {
                println!(
                    "{cf}: keys~{} sst={}MB",
                    e.store.property(cf, "rocksdb.estimate-num-keys").unwrap_or(0),
                    e.store.property(cf, "rocksdb.total-sst-files-size").unwrap_or(0) >> 20
                );
            }
        }
    }
    Ok(())
}
