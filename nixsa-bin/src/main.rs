use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use libc::{signal, SIGINT, SIG_IGN};
use shell_quote::{Bash, QuoteRefExt};
use std::collections::{HashSet, VecDeque};
use std::os::unix::{fs::symlink, process::ExitStatusExt};
use std::process::{Command, ExitCode};
use std::{env, fs};
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

const DESCRIPTION: &str = "\
Usage:
nixsa [options] [cmd [arg [arg ...]]

Run a command in the Nixsa (Nix Standalone) environment.

Assuming NIXSA is the Nixsa folder, meaning NIXSA/nixsa.toml exists, will use
bwrap to run the command with NIXSA/nix binded to /nix.

The Nixsa folder is found by using /proc/self/exe to find the canonical path
of the nixsa executable, and going upwards until a directory which contains
`nixsa.toml` is found.

If basename(argv[0]) is not 'nixsa', meaning that we run by a symlink,
basename(argv[0]) will be used as the command, and no argument parsing is done.
So, if NIXSA/bin/nix is a symlink to `nixsa`, running `NIXSA/bin/nix --help`
is the same as running `NIXSA/bin/nixsa nix --help`.

If no arguments are given, and basename(argv[0]) is 'nixsa', $SHELL will be used
as the command.

After running the command, the entries in the NIXSA/bin directories will be
updated with symlinks to `nixsa` according to the entries in
NIXSA/state/profile/bin. This will only be done if NIXSA/bin was modified
before NIXSA/state/profile, so the update will be skipped if the profile
wasn't updated.

Options:
  -h, --help       show this help message and exit.
  -s, --symlinks   update symlinks, regardless of modification time, and exit.
  -v, --verbose    show the commands which are run.
";

fn verify_bwrap() -> Result<()> {
    let output = Command::new("bwrap").arg("--version").output();
    if output.is_err() {
        bail!("Couldn't run `bwrap --version`. bubblewrap is probably not installed. Try: sudo apt install bubblewrap")
    }
    Ok(())
}

fn get_bwrap_prefix(nixpath: &Utf8Path) -> Result<Vec<String>> {
    let mut args: Vec<String> = vec!["bwrap".into(), "--bind".into(), nixpath.to_string(), "/nix".into()];
    args.extend(["--proc".into(), "/proc".into(), "--dev".into(), "/dev".into()]);
    for root_dir in Utf8PathBuf::from("/").read_dir_utf8()?.flatten() {
        let root_dir = root_dir.path();
        let file_name = root_dir.file_name().unwrap_or_default();
        if file_name != "dev" && file_name != "proc" && file_name != "nix" && root_dir.exists() {
            args.extend(["--bind".into(), root_dir.to_string(), root_dir.to_string()]);
        }
    }
    if let Ok(val) = std::env::var("NIXSA_BWRAP_ARGS") {
        args.extend(val.split_whitespace().map(String::from));
    }
    Ok(args)
}

/// Get the real path to the 'bin' dir in the active profile, resolving `/nix` symlinks
fn get_real_profile_bin_dir(basepath: &Utf8Path) -> Result<Utf8PathBuf> {
    let profiles_dir = basepath.join("state/profiles");
    let cur_profile_base = profiles_dir.join("profile").read_link_utf8()?;
    let cur_profile = profiles_dir.join(cur_profile_base);
    let cur_profile_nix = cur_profile.read_link_utf8()?;
    let cur_profile_nix_stripped = cur_profile_nix.strip_prefix("/nix/")?;
    let cur_profile_real = basepath.join("nix").join(cur_profile_nix_stripped);
    let cur_profile_bin = cur_profile_real.join("bin");
    let cur_profile_bin_real = if cur_profile_bin.is_symlink() {
        let cur_profile_bin_nix = cur_profile_bin.read_link_utf8()?;
        let cur_profile_bin_nix_stripped = cur_profile_bin_nix.strip_prefix("/nix/")?;
        basepath.join("nix").join(cur_profile_bin_nix_stripped)
    } else {
        cur_profile_bin
    };
    if !cur_profile_bin_real.is_dir() {
        bail!("{:?} is not a directory", cur_profile_bin_real);
    }
    Ok(cur_profile_bin_real)
}

fn read_profile_bin_dir(profile_bin_dir: &Utf8Path) -> Result<(HashSet<String>, Utf8PathBuf)> {
    let mut src_names = HashSet::<String>::new();
    let mut nixsa_link = Option::<Utf8PathBuf>::None;
    for entry in profile_bin_dir.read_dir_utf8()? {
        let name: String = entry?.file_name().into();
        if name == "nixsa" {
            let link = profile_bin_dir.join("nixsa").read_link_utf8()?;
            if !link.as_str().starts_with("/nix/store/") {
                bail!("Expecting `nixsa` symlink in profile dir to start with `/nix/store`, is {}", link);
            }
            nixsa_link = Some(link);
        } else {
            src_names.insert(name);
        }
    }
    let nixsa_link = match nixsa_link {
        None => {
            bail!("The profile bin directory doesn't contain a `nixsa` entry. Not updating bin/ symlinks.")
        }
        Some(link) => link,
    };
    Ok((src_names, nixsa_link))
}

fn read_nixsa_bin_dir(nixsa_bin_dir: &Utf8Path) -> Result<(HashSet<String>, Option<Utf8PathBuf>)> {
    let mut dst_names = HashSet::<String>::new();
    let mut cur_nixsa_link = Option::<Utf8PathBuf>::None;
    for entry in nixsa_bin_dir.read_dir_utf8()? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if !path.is_symlink() {
            bail!("Expecting all items in bin dir to be symlinks, {:?} is not a symlink", path);
        }
        let link = path.read_link_utf8()?;
        if name == "nixsa" {
            cur_nixsa_link = Some(link);
        } else {
            if link != "nixsa" {
                bail!("Expecting all items in bin dir to be symlinks to 'nixsa', {:?} is not", path);
            }
            dst_names.insert(name.into());
        }
    }
    Ok((dst_names, cur_nixsa_link))
}

/// Update the symlinks in the nixsa/bin directory based on the profile bin directory
fn update_bin_dir(basepath: &Utf8Path, ignore_mtime: bool) -> Result<()> {
    let profiles_dir = basepath.join("state/profiles");
    let profiles_mtime = profiles_dir.metadata()?.modified()?;
    let nixsa_bin_dir = basepath.join("bin");
    if !ignore_mtime {
        let nixsa_bin_mtime = nixsa_bin_dir.metadata()?.modified()?;
        if nixsa_bin_mtime >= profiles_mtime {
            info!("bin dir modification time is later than the state/profiles mtime, skipping symlink sync.");
            return Ok(());
        }
    }

    let profile_bin_dir = get_real_profile_bin_dir(basepath)?;
    let (src_names, nixsa_link) = read_profile_bin_dir(&profile_bin_dir)?;
    let (dst_names, cur_nixsa_link) = read_nixsa_bin_dir(&nixsa_bin_dir)?;

    let nixsa_rel_link = Utf8PathBuf::from("../").join(&nixsa_link.as_str()[1..]);
    if !nixsa_bin_dir.join(&nixsa_rel_link).exists() {
        bail!("nixsa link in profile doesn't exist: {}", nixsa_bin_dir.join(&nixsa_rel_link));
    }

    let cur_nixsa_link_uptodate = cur_nixsa_link.as_ref().is_some_and(|link| *link == nixsa_rel_link);
    if src_names == dst_names && cur_nixsa_link_uptodate {
        info!("nixsa/bin directory is up to date with profile/bin directory.");
    } else {
        for name in dst_names.difference(&src_names) {
            let path = nixsa_bin_dir.join(name);
            info!("Removing symlink {:?}", path);
            fs::remove_file(path)?;
        }
        for name in src_names.difference(&dst_names) {
            let path = nixsa_bin_dir.join(name);
            info!("Creating symlink {:?} -> nixsa", path);
            symlink("nixsa", path)?;
        }
        if !cur_nixsa_link_uptodate {
            let path = nixsa_bin_dir.join("nixsa");
            if cur_nixsa_link.is_some() {
                info!("Removing symlink {:?}", path);
                fs::remove_file(&path)?;
            }
            info!("Creating symlink {:?} -> {:?}", path, nixsa_rel_link);
            symlink(nixsa_rel_link, &path)?;
            assert!(path.exists());
        }
    }
    Ok(())
}

fn quote(s: &str) -> String {
    s.quoted(Bash)
}

fn ignore_sigint() {
    unsafe {
        signal(SIGINT, SIG_IGN);
    }
}

fn nixsa(basepath: &Utf8Path, cmd: &str, args: &[String]) -> Result<ExitCode> {
    verify_bwrap()?;
    ignore_sigint();

    let nixpath = basepath.join("nix");
    let bwrap_prefix = get_bwrap_prefix(&nixpath)?;
    let nix_sh = basepath.join("state/profile/etc/profile.d/nix.sh");
    let bash_c = format!("source {} && exec {} \"$@\"", quote(nix_sh.as_str()), quote(cmd));

    let mut args1 = bwrap_prefix;
    args1.extend(["bash".into(), "-c".into(), bash_c, "--".into()]);
    args1.extend(args.iter().map(String::clone));

    let extra_env = [
        ("NIX_USER_CONF_FILES", basepath.join("config/nix.conf")),
        ("NIX_CACHE_HOME", basepath.join("cache")),
        ("NIX_CONFIG_HOME", basepath.join("config")),
        ("NIX_DATA_HOME", basepath.join("share")),
        ("NIX_STATE_HOME", basepath.join("state")),
    ];

    info!(
        "{} {}",
        extra_env.iter().map(|(name, val)| format!("{}={}", name, val)).collect::<Vec<String>>().join(" "),
        args1.iter().map(|s| quote(s)).collect::<Vec<String>>().join(" ")
    );

    let status = Command::new(&args1[0]).args(&args1[1..]).envs(extra_env).status()?;
    update_bin_dir(basepath, false)?;
    let code = u8::try_from(match status.code() {
        Some(code) => code,
        None => {
            let signal = status.signal().expect("signal should not be None if code is None");
            warn!("Subprocess killed with signal {}", signal);
            signal
        }
    })
    .expect("Code should fit u8");
    Ok(ExitCode::from(code))
}

fn find_nixsa_root(path: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
    let mut path = path;
    loop {
        match path.parent() {
            None => return Ok(None),
            Some(p) => {
                path = p;
                if path.join("nixsa.toml").try_exists()? {
                    return Ok(Some(path.into()));
                }
            }
        }
    }
}

enum ParsedArgs {
    Help,
    Symlinks { basepath: Utf8PathBuf },
    Run { basepath: Utf8PathBuf, cmd: String, args: VecDeque<String>, verbose: bool },
}

fn parse_args(argv0: String, path: String, args: VecDeque<String>) -> Result<ParsedArgs> {
    let proc_self_exe: &Utf8Path = "/proc/self/exe".into();
    let exe_realpath = proc_self_exe.read_link_utf8()?;
    let root = find_nixsa_root(&exe_realpath)?;
    let name = <&Utf8Path>::from(argv0.as_str()).file_name().context("Expecting argv[0] to have a final element")?;
    match root {
        None => {
            if args.len() > 1 && (args[1] == "-h" || args[1] == "--help") {
                Ok(ParsedArgs::Help)
            } else {
                bail!("Couldn't find a directory containing {} which contains a `nixsa.toml` file.", proc_self_exe);
            }
        }
        Some(basepath) => {
            let nixpath = basepath.join("nix");
            if !nixpath.is_dir() {
                bail!("{:?} doesn't exist or is not a directory", nixpath);
            }
            let profile_path = basepath.join("state/profile");
            if !profile_path.is_symlink() {
                bail!("{:?} is not a symlink", profile_path);
            }
            if name != "nixsa" {
                Ok(ParsedArgs::Run { basepath, cmd: name.into(), args, verbose: false })
            } else {
                if args.len() > 1 && (args[1] == "-h" || args[1] == "--help") {
                    return Ok(ParsedArgs::Help);
                }

                if args.len() > 1 && (args[1] == "-s" || args[1] == "--symlinks") {
                    return Ok(ParsedArgs::Symlinks { basepath });
                }

                let mut args = args;
                let verbose: bool;
                if args.len() > 1 && (args[1] == "-v" || args[1] == "--verbose") {
                    verbose = true;
                    args.remove(1);
                } else {
                    verbose = false;
                }

                if args.len() == 0 {
                    args.push_front(env::var("SHELL")?);
                }

                Ok(ParsedArgs::Run { basepath, cmd: args[1].clone(), args: args[2..].into(), verbose })
            }
        }
    }
}

fn main() -> Result<ExitCode> {
    let mut args0: VecDeque<String> = env::args().collect();
    let program_name = args0.pop_front().unwrap();
    let path = args0.pop_front().context("Expected nixsa path to be the first argument")?;
    let args = parse_args(program_name, path, args0)?;
    match args {
        ParsedArgs::Help => {
            print!("{}", DESCRIPTION);
            Ok(ExitCode::from(0))
        }
        ParsedArgs::Symlinks { basepath } => {
            let subscriber = FmtSubscriber::builder().with_max_level(Level::INFO).without_time().finish();
            tracing::subscriber::set_global_default(subscriber)?;

            update_bin_dir(&basepath, true)?;
            Ok(ExitCode::from(0))
        }
        ParsedArgs::Run { basepath, cmd, args, verbose } => {
            let max_level = if verbose { Level::INFO } else { Level::WARN };
            let subscriber = FmtSubscriber::builder().with_max_level(max_level).without_time().finish();
            tracing::subscriber::set_global_default(subscriber)?;

            nixsa(&basepath, &cmd, &args)
        }
    }
}
