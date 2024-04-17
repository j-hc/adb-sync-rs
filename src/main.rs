use adb_sink::adb::AdbErr;
use adb_sink::adb::AdbShell;
use adb_sink::adb_cmd;
use adb_sink::fs::AsStr;
use adb_sink::fs::{AndroidFS, FileSystem, LocalFS, SyncFile};
use adb_sink::log;
use clap::{Args, Parser, Subcommand};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use typed_path::{UnixPath, UnixPathBuf};

#[derive(Args, Debug)]
#[command(arg_required_else_help(true))]
struct PullPushArgs {
    source: PathBuf,
    dest: PathBuf,

    /// set modified time of files
    #[arg(short = 't', long)]
    set_times: bool,

    /// delete files on target that does not exist in source
    #[arg(short = 'd', long)]
    delete_if_dne: bool,

    /// ignore dirs starting with specified string
    #[arg(short, long)]
    ignore_dir: Vec<Box<str>>,
}

#[derive(Debug, Subcommand)]
enum SubCmds {
    Pull(PullPushArgs),
    Push(PullPushArgs),
}

#[derive(Parser, Debug)]
#[command(
    help_template = "{author-with-newline}{about-section}Version: {version}\n{usage-heading} \
    {usage}\n{all-args} {tab}"
)]
#[command(arg_required_else_help(true))]
#[clap(version = "1.0", author = "github.com/j-hc")]
struct Cli {
    #[clap(subcommand)]
    subcmd: SubCmds,
}

type DirFileMap = HashMap<UnixPathBuf, HashSet<SyncFile>>;
fn get_dir_file_map(fs: Vec<SyncFile>, dir: &UnixPath) -> anyhow::Result<DirFileMap> {
    let mut dir_file_map: DirFileMap = HashMap::new();
    for f in fs {
        let mut p = f
            .path
            .strip_prefix(dir)
            .expect("has the prefix")
            .to_path_buf();
        p.pop();
        dir_file_map.entry(p).or_default().insert(f);
    }
    Ok(dir_file_map)
}

#[derive(Clone, Copy)]
enum SetMtime {
    WithAdb,
    WithMtime,
    None,
}

fn adb_with_reason(
    adb_command: &str,
    af: &SyncFile,
    lf_path: &UnixPath,
    reason: &str,
    set_mtime: SetMtime,
    dest_fs: &mut impl FileSystem,
) -> anyhow::Result<()> {
    let lf_str = lf_path.as_str();
    let af_str = af.path.as_str();
    let op = match set_mtime {
        SetMtime::WithAdb => adb_cmd!(adb_command, "-a", af_str, lf_str)?,
        SetMtime::WithMtime => {
            let op = adb_cmd!(adb_command, af_str, lf_str)?;
            dest_fs.set_mtime(lf_path, af.timestamp)?;
            op
        }
        SetMtime::None => adb_cmd!(adb_command, af_str, lf_str)?,
    };
    log!("{adb_command} ({reason}) {}", op.trim_end());
    Ok(())
}

fn pull_push<SRC: FileSystem, DEST: FileSystem>(
    src_fs: &mut SRC,
    dest_fs: &mut DEST,
    PullPushArgs {
        source,
        dest,
        delete_if_dne,
        ignore_dir,
        set_times,
    }: PullPushArgs,
    adb_command: &'static str,
) -> anyhow::Result<()> {
    let source_file_name = source.file_name().unwrap().to_str().unwrap().to_string();

    let source = typed_path::PathBuf::<typed_path::NativeEncoding>::try_from(source)
        .unwrap()
        .with_unix_encoding();
    let mut dest = typed_path::PathBuf::<typed_path::NativeEncoding>::try_from(dest)
        .unwrap()
        .with_unix_encoding();

    dest.push(source_file_name);
    if adb_command == "pull" && !PathBuf::from_str(&dest.to_string()).unwrap().exists() {
        LocalFS.mkdir(&UnixPathBuf::try_from(dest.clone()).unwrap())?;
    }
    log!("{} -> {}\n", source.display(), dest.display());

    let mut setmtime = SetMtime::None;
    if set_times {
        if adb_command == "pull" {
            setmtime = SetMtime::WithAdb;
        } else {
            setmtime = SetMtime::WithMtime;
        }
    }

    let (src_files, mut src_empty_dirs) = src_fs.get_all_files(&source)?;
    src_empty_dirs.retain(|dir| {
        !ignore_dir.iter().any(|g| {
            dir.path
                .strip_prefix(&source)
                .unwrap()
                .as_str()
                .starts_with(&**g)
        })
    });
    let dir_file_map_android = get_dir_file_map(src_files, &source)?;

    let (dest_files, dest_empty_dirs) = dest_fs.get_all_files(&dest)?;
    let mut dir_file_map_local = get_dir_file_map(dest_files, &dest)?;

    for (path, androidfs) in dir_file_map_android {
        let localfs = dir_file_map_local.remove(&path);
        if ignore_dir.iter().any(|g| path.as_str().starts_with(&**g)) {
            log!("SKIP DIR (IGNORED): {}", path.display());
            continue;
        }
        if localfs.is_none() {
            dest_fs.mkdir(&dest.join(&path))?;
        }

        for af in &androidfs {
            let lf = localfs.as_ref().and_then(|localfs| localfs.get(af));
            match lf {
                Some(lf) if af.size != lf.size => {
                    adb_with_reason(adb_command, af, &lf.path, "SIZE", setmtime, dest_fs)?
                }
                Some(lf) if af.timestamp > lf.timestamp => {
                    adb_with_reason(adb_command, af, &lf.path, "NEWER", setmtime, dest_fs)?
                }
                Some(_) => (), //log!("SKIP: '{}'", af.path.display()),
                None => adb_with_reason(
                    adb_command,
                    af,
                    &dest.join(&path).join(&*af.name),
                    "DNE",
                    setmtime,
                    dest_fs,
                )?,
            }
        }
        if delete_if_dne {
            if let Some(localfs) = localfs {
                for sf_del in localfs.difference(&androidfs) {
                    // windows does not support file names ending with .
                    let mut c = sf_del.clone();
                    let mut c_name = c.name.to_string();
                    c_name.push('.');
                    c.name = c_name.into();
                    if androidfs.get(&c).is_none() {
                        log!("DEL (DNE): '{}'", sf_del.path.display());
                        dest_fs.rm_file(&sf_del.path)?;
                    }
                }
            }
        }
    }
    let empty_dirs_hs = |empty_dirs: Vec<SyncFile>, prefix| -> HashSet<Box<UnixPath>> {
        HashSet::from_iter(
            empty_dirs
                .into_iter()
                .map(|p| p.path.strip_prefix(prefix).unwrap().into()),
        )
    };
    let dest_empty_dirs_hs = empty_dirs_hs(dest_empty_dirs, &dest);
    let src_empty_dirs_hs = empty_dirs_hs(src_empty_dirs, &source);
    for sf_dest_dir_empty in src_empty_dirs_hs.difference(&dest_empty_dirs_hs) {
        let p = dest.join(sf_dest_dir_empty);
        dest_fs.mkdir(&p)?;
    }
    if delete_if_dne {
        for remaining_local in dir_file_map_local.keys() {
            let p = dest.join(remaining_local);
            log!("DEL DIR: '{}'", p.display());
            let _ = dest_fs
                .rm_dir(&p)
                .map_err(|e| log!("could not delete: '{}'", e));
        }
        for sf_dest_dir_empty in dest_empty_dirs_hs.difference(&src_empty_dirs_hs) {
            let sf_dest_dir_empty = dest.join(sf_dest_dir_empty);
            log!("DEL EMPTY DIR: '{}'", sf_dest_dir_empty.display());
            let _ = dest_fs
                .rm_dir(&sf_dest_dir_empty)
                .map_err(|e| log!("could not delete: '{}'", e));
        }
    }
    Ok(())
}

fn run() -> anyhow::Result<()> {
    let args = Cli::parse();
    match adb_cmd!("devices") {
        Ok(devices) => {
            println!("{}\n", devices.trim());
            if devices
                .lines()
                .filter(|line| line.contains("\tdevice"))
                .count()
                > 1
            {
                anyhow::bail!("more than 1 device connected");
            }
        }
        Err(AdbErr::IO(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("adb binary not found")
        }
        Err(e) => anyhow::bail!("{}", e),
    }

    let mut android_fs = AndroidFS {
        shell: AdbShell::new()?,
    };
    match args.subcmd {
        SubCmds::Pull(p) => {
            pull_push::<AndroidFS, LocalFS>(&mut android_fs, &mut LocalFS, p, "pull")?;
        }
        SubCmds::Push(p) => {
            pull_push::<LocalFS, AndroidFS>(&mut LocalFS, &mut android_fs, p, "push")?
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {}", e);
            eprintln!("Backtrace:\n{}", e.backtrace());
            ExitCode::FAILURE
        }
    }
}
