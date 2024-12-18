use std::{
	ffi::{c_int, c_void, CString},
	fs::File,
	io::{Seek, Write},
	path::{Path, PathBuf},
	process::{Command, Stdio},
};

use anyhow::{anyhow, bail, Context, Result};
use libc::{close, open, O_NONBLOCK, O_RDONLY};
use log::{debug, info};
use termsize::Size;

use crate::{context::ImageVariant, device::DeviceArch};

#[link(name = "c")]
extern "C" {
	#[allow(dead_code)]
	pub fn geteuid() -> c_int;
	#[allow(dead_code)]
	pub fn sync() -> c_void;
	pub fn syncfs(fd: c_int) -> c_int;
}

#[cfg(not(debug_assertions))]
const AB_DIR: &str = "/usr/share/aoscbootstrap";
const DEFAULT_GROUPS: &[&str] = &["audio", "video", "cdrom", "plugdev", "tty", "wheel"];
const LOCALCONF_PATH: &str = "etc/locale.conf";
const BINFMT_DIR: &str = "/proc/sys/fs/binfmt_misc";

/// Create a sparse file with specified size in bytes.
pub fn get_sparse_file<P: AsRef<Path>>(path: P, size: u64) -> Result<File> {
	let img_path = path.as_ref();
	let parent = img_path.parent().unwrap_or(Path::new("/"));
	if !parent.exists() {
		return Err(anyhow!(
			"One or more of the parent directories does not exist"
		));
	}
	debug!(
		"Creating sparse file at '{}' with size {} bytes ...",
		&img_path.display(),
		size
	);
	let mut img_file = File::create_new(img_path).context(format!(
		"Error creating raw image file '{}'",
		&img_path.display()
	))?;
	// Seek to the desired size
	img_file.seek(std::io::SeekFrom::Start(size - 1))?;
	// Write zero at the end of file to punch a hole
	img_file.write_all(&[0]).context(
		"Failed to punch hole for sparse file. Does your filesystem support sparse files?",
	)?;
	img_file.sync_all()?;
	Ok(img_file)
}

pub fn create_sparse_file<P: AsRef<Path>>(path: P, size: u64) -> Result<()> {
	get_sparse_file(path, size)?;
	Ok(())
}

/// Tell kernel to reread the partition table.
pub fn refresh_partition_table<P: AsRef<Path>>(dev: P) -> Result<()> {
	debug!("Refreshing partition table ...");
	let dev = dev.as_ref();
	let mut command = Command::new("partprobe");
	let command = command.arg("--summary").arg(dev).stdout(Stdio::piped());
	let out = command
		.output()
		.context("Failed to run partprobe(8) to refresh the partition table")?
		.stdout;
	info!("partprobe: {}", String::from_utf8_lossy(&out).trim());
	Ok(())
}

#[cfg(debug_assertions)]
#[allow(dead_code)]
#[allow(unused_variables)]
pub fn bootstrap_distribution<P: AsRef<Path>, S: AsRef<str>>(
	variant: &ImageVariant,
	path: P,
	arch: DeviceArch,
	mirror: S,
) -> Result<()> {
	use std::fs;

	const DIRS: &[&str] = &[
		"bin",
		"etc",
		"lib",
		"usr",
		"usr/bin",
		"usr/lib",
		"usr/share",
		"var",
	];
	const OS_RELEASE: &str = r#"PRETTY_NAME="AOSC OS (12.0.0)"
NAME="AOSC OS"
VERSION_ID="12.0.0"
VERSION="12.0.0 (localhost)"
BUILD_ID="20241128"
ID=aosc
ANSI_COLOR="1;36"
HOME_URL="https://aosc.io/"
SUPPORT_URL="https://github.com/AOSC-Dev/aosc-os-abbs"
BUG_REPORT_URL="https://github.com/AOSC-Dev/aosc-os-abbs/issues""#;

	let path = path.as_ref();
	info!(
		"Bootstrapping {} system distribution to {} ...",
		variant,
		path.display()
	);
	for d in DIRS {
		let p = path.join(d);
		debug!("Creating directory {}", p.display());
		fs::create_dir_all(p)?;
	}
	let mut fd = File::create(path.join("etc/os-release"))?;
	fd.write_all(OS_RELEASE.as_bytes())?;
	info!("Successfully bootstrapped {} distribution.", variant);
	Ok(())
}

#[cfg(not(debug_assertions))]
/// Run aoscbootstrap to generate a system release
pub fn bootstrap_distribution<P: AsRef<Path>, S: AsRef<str>>(
	variant: &ImageVariant,
	path: P,
	arch: DeviceArch,
	mirror: S,
) -> Result<()> {
	use termsize::Size;
	let path = path.as_ref();
	let mirror = mirror.as_ref();

	// Display a progressbar
	let term_geometry = termsize::get().unwrap_or(Size { rows: 25, cols: 80 });
	// Set up the scroll region
	eprint!("\n\x1b7\x1b[0;{}r\x1b8\x1b[1A", term_geometry.rows - 1);
	eprint!("\x1b7\x1b[{};0f\x1b[102m\x1b[0K\x1b[2K", term_geometry.rows);
	eprint!(
		"\x1b[30m[{}] Bootstrapping release ...",
		variant.to_string().to_lowercase()
	);
	eprint!("\x1b8\x1b[0m");

	info!(
		"Bootstrapping {} system distribution to {} ...",
		variant,
		path.display()
	);
	let mut command = Command::new("aoscbootstrap");
	let command = command
		.arg("stable")
		.arg(path)
		.arg(mirror)
		.arg("-x")
		.args([
			"--config",
			&format!("{}/{}", AB_DIR, "config/aosc-mainline.toml"),
		])
		.args(["--arch", &arch.to_string().to_lowercase()])
		.args(["-s", &format!("{}/{}", AB_DIR, "scripts/reset-repo.sh")])
		.args(["-s", &format!("{}/{}", AB_DIR, "scripts/enable-dkms.sh")])
		.args([
			"--include-files",
			&format!(
				"{}/recipes/{}.lst",
				AB_DIR,
				variant.to_string().to_lowercase()
			),
		]);

	debug!("Runnig command {:?} ...", command);
	let status = command.status().context("Failed to run aoscbootstrap")?;
	// Recover the terminal
	restore_term();
	if status.success() {
		info!(
			"Successfully bootstrapped {} distribution.",
			variant.to_string()
		);
		Ok(())
	} else if let Some(c) = status.code() {
		Err(anyhow!("aoscbootstrap exited unsuccessfully (code {})", c))
	} else {
		Err(anyhow!("rsync exited abnormally"))
	}
}

pub fn rsync_sysroot<P: AsRef<Path>>(src: P, dst: P) -> Result<()> {
	let src = src.as_ref();
	let dst = dst.as_ref();
	if !src.is_dir() || !dst.is_dir() {
		bail!("Neither directory exists.");
	}
	info!(
		"Installing the distribution in {} to {} ...",
		src.display(),
		dst.display()
	);
	let mut command = Command::new("rsync");
	command.args(["-axAHXSW", "--numeric-ids", "--info=progress2", "--no-i-r"]);
	command.arg(format!("{}/", src.to_string_lossy()));
	command.arg(format!("{}/", dst.to_string_lossy()));
	debug!("Running command {:?}", command);
	// return Ok(());
	let status = command.status().context("Failed to run rsync")?;
	if status.success() {
		Ok(())
	} else if let Some(s) = status.code() {
		Err(anyhow!("rsync exited with non-zero status {}", s))
	} else {
		Err(anyhow!("rsync exited abnormally"))
	}
}

/// Recover the terminal
#[inline]
pub fn restore_term() {
	let term_geometry = termsize::get().unwrap_or(Size { rows: 25, cols: 80 });
	eprint!(
		"\x1b7\x1b[0;{}r\x1b[{};0f\x1b[0K\x1b8",
		term_geometry.rows, term_geometry.rows
	);
}

#[allow(dead_code)]
pub fn sync_all() -> Result<()> {
	let _ = unsafe { sync() };
	Ok(())
}

/// Sync the filesystem behind the path.
pub fn sync_filesystem(path: &dyn AsRef<Path>) -> Result<()> {
	let tgt_path = path.as_ref();
	let path = CString::new(tgt_path.as_os_str().as_encoded_bytes())?;
	let path_ptr: *const i8 = path.as_ptr();

	let fd = unsafe { open(path_ptr, O_RDONLY | O_NONBLOCK) };
	if fd < 0 {
		let errno = errno::errno();
		return Err(anyhow!(
			"Failed to open path {}: {}",
			&tgt_path.display(),
			errno
		));
	}
	debug!("open(\"{}\") returned fd {}", &tgt_path.display(), fd);
	let result = unsafe { syncfs(fd) };
	debug!("syncfs({}) returned {}", fd, result);
	if result != 0 {
		let close = unsafe { close(fd) };
		if close != 0 {
			panic!("Failed to close fd {}: {}", fd, errno::errno());
		}
		let errno = errno::errno();
		return Err(anyhow!(
			"Failed to sync filesystem {}: {}",
			tgt_path.display(),
			errno
		));
	}
	let close = unsafe { close(fd) };
	if close != 0 {
		panic!("Failed to close fd {}: {}", fd, errno::errno());
	}
	Ok(())
}

pub fn add_user<S: AsRef<str>, P: AsRef<Path>>(
	root: P,
	name: S,
	password: S,
	comment: Option<S>,
	homedir: Option<P>,
	groups: Option<&[&str]>,
) -> Result<()> {
	// shadow does not expose such functionality through a library,
	// we have to invoke commands to achieve this.
	let name = name.as_ref();
	let root = root.as_ref().to_string_lossy();
	let password = password.as_ref();
	let comment = comment.as_ref();
	let homedir = if let Some(h) = homedir {
		PathBuf::from(h.as_ref())
	} else {
		PathBuf::from("/home").join(name)
	};
	let homedir = homedir.to_string_lossy();
	let groups = if let Some(g) = groups {
		g
	} else {
		DEFAULT_GROUPS
	};
	let groups = groups.join(",");
	let mut cmd_useradd = Command::new("useradd");
	let mut cmd_chpasswd = Command::new("chpasswd");
	cmd_useradd
		.args(["-R", &root])
		.args(["-m", "-d", &homedir])
		.args(["-G", &groups]);
	if let Some(c) = comment {
		cmd_useradd.args(["-c", c.as_ref()]);
	}
	cmd_useradd.arg(name);
	cmd_chpasswd.stdin(Stdio::piped()).args(["-R", &root]);
	let result = cmd_useradd.status().context("Failed to run useradd")?;
	if !result.success() {
		if let Some(c) = result.code() {
			bail!("useradd exited with status {}", c);
		} else {
			bail!("useradd exited abnormally");
		}
	}
	let mut chpasswd_proc = cmd_chpasswd.spawn().context("Failed to run chpasswd")?;
	let chpasswd_stdin = chpasswd_proc
		.stdin
		.as_mut()
		.context("Failed to open stdin for chpasswd")?;
	// echo "$name:$password" | chpasswd -R /target/root
	let chpasswd_buf = format!("{}:{}", name, password);
	chpasswd_stdin.write_all(chpasswd_buf.as_bytes())?;
	chpasswd_proc.wait()?;
	Ok(())
}

pub fn set_locale<S: AsRef<str>, P: AsRef<Path>>(root: P, locale: S) -> Result<()> {
	let root = root.as_ref();
	let locale = locale.as_ref();
	let locale_conf_path = root.join(LOCALCONF_PATH);
	let locale = format!("LANG=\"{}\"", locale);
	let mut locale_conf_fd = File::options()
		.write(true)
		.truncate(true)
		.create(true)
		.open(locale_conf_path)?;
	locale_conf_fd.write_all(locale.as_bytes())?;
	locale_conf_fd.sync_all()?;
	Ok(())
}

pub fn check_binfmt(arch: &DeviceArch) -> Result<()> {
	if arch.is_native() {
		return Ok(());
	}
	let name = arch.get_qemu_binfmt_names();
	let binfmt_path = Path::new(BINFMT_DIR);
	if !binfmt_path.is_dir() {
		bail!("binfmt_misc support is currently not available on your system. Cannot continue.")
	}
	let path = binfmt_path.join(name);
	if !path.is_file() {
		bail!("{} is not found in registered binfmt_misc targets.\nPlease make sure QEMU static and binfmt file for this target are installed.", name);
	}
	Ok(())
}

pub fn run_str_script_with_chroot(
	root: &dyn AsRef<Path>,
	script: &str,
	shell: Option<&dyn AsRef<str>>,
) -> Result<()> {
	let mut cmd = Command::new("chroot");
	let shell = if let Some(s) = shell {
		s.as_ref()
	} else {
		"/bin/bash"
	};
	// Let's assume all shells supports "-c SCRIPT".
	// But I think it is better to pipe into the shell's stdin.
	cmd.args([&root.as_ref().to_string_lossy(), shell, "-c", "--", script]);
	let result = cmd.status().context("Failed to run chroot")?;
	if result.success() {
		Ok(())
	} else if let Some(c) = result.code() {
		bail!(
			"The following command failed with exit status {}:\n{:?}",
			c,
			&cmd
		)
	} else {
		bail!("The following command exited abnormally:\n{:?}", &cmd)
	}
}

pub fn run_script_with_chroot<P: AsRef<Path>>(
	root: P,
	script: P,
	shell: Option<&dyn AsRef<str>>,
) -> Result<()> {
	let mut cmd = Command::new("chroot");
	let shell = if let Some(s) = shell {
		s.as_ref()
	} else {
		"/bin/bash"
	};
	// Let's assume all shells supports "-c SCRIPT".
	// But I think it is better to pipe into the shell's stdin.
	cmd.args([
		&root.as_ref().to_string_lossy(),
		shell,
		"--",
		&script.as_ref().to_string_lossy(),
	]);
	let result = cmd.status().context("Failed to run chroot")?;
	if result.success() {
		Ok(())
	} else if let Some(c) = result.code() {
		bail!(
			"The following command failed with exit status {}:\n{:?}",
			c,
			&cmd
		)
	} else {
		bail!("The following command exited abnormally:\n{:?}", &cmd)
	}
}
