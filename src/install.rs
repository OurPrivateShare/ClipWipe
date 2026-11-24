use crate::{Result, WatchOptions};
use sha2::{Digest, Sha256};
use std::{
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(any(target_os = "linux", test))]
fn absolute_env(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| format!("环境变量 {name} 缺少绝对路径").into())
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => Err(format!("路径不是普通文件：{}", path.display()).into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn temporary_path(parent: &Path) -> Result<PathBuf> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)?;
    Ok(parent.join(format!(".clipwipe-{:032x}.tmp", u128::from_le_bytes(bytes))))
}

const EXECUTABLE: u32 = 0o755;
const SHARED_RECORD: u32 = 0o644;

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or("文件路径缺少父目录")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn publish(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or("文件路径缺少父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(parent)?;
    let mut file = fs::File::create_new(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(windows)]
        let _ = mode;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        // Persist the rename before the next step publishes a new recovery record.
        #[cfg(unix)]
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

const RECORD_HEADER: &str = "ClipWipe installed file v1";
fn record(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .expect("fixed installation filename")
        .to_string_lossy();
    path.with_file_name(format!(".{name}.sha256"))
}

fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn fingerprints(path: &Path) -> Result<Vec<String>> {
    let receipt = record(path);
    if !regular_file(&receipt)? {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&receipt)?;
    let mut lines = text.lines();
    let valid_header = lines.next() == Some(RECORD_HEADER);
    let hashes: Vec<_> = lines.map(str::to_owned).collect();
    if !valid_header
        || hashes.is_empty()
        || hashes.len() > 2
        || !hashes
            .iter()
            .all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(format!("无法识别安装记录：{}", receipt.display()).into());
    }
    Ok(hashes)
}

fn inspect(path: &Path, adopting: Option<&str>) -> Result<Option<String>> {
    let hashes = fingerprints(path)?;
    if !regular_file(path)? {
        return Ok(None);
    }
    let hash = digest(&fs::read(path)?);
    if hashes.contains(&hash) || adopting == Some(&hash) {
        Ok(Some(hash))
    } else {
        Err(format!("文件未经本程序安装或已被修改，请检查：{}", path.display()).into())
    }
}

fn install_file(path: &Path, data: &[u8]) -> Result<()> {
    let new_hash = digest(data);
    let old = inspect(path, Some(&new_hash))?;
    let receipt = record(path);
    let final_record = format!("{RECORD_HEADER}\n{new_hash}\n");
    let replace = old.as_ref() != Some(&new_hash);
    if replace {
        // Record both versions before publishing so a retry can recover on either side of rename.
        let pending = match old {
            Some(hash) => format!("{RECORD_HEADER}\n{hash}\n{new_hash}\n"),
            None => final_record.clone(),
        };
        publish(&receipt, pending.as_bytes(), SHARED_RECORD)?;
        publish(path, data, EXECUTABLE)?;
    }
    #[cfg(unix)]
    if !replace {
        use std::os::unix::fs::PermissionsExt;
        let file = fs::File::open(path)?;
        let mut permissions = file.metadata()?.permissions();
        if permissions.mode() & 0o7777 != EXECUTABLE {
            permissions.set_mode(EXECUTABLE);
            file.set_permissions(permissions)?;
        }
    }
    publish(&receipt, final_record.as_bytes(), SHARED_RECORD)
}

fn remove_if_present(path: &Path) -> Result<()> {
    if regular_file(path)? {
        fs::remove_file(path)?;
        #[cfg(unix)]
        sync_parent(path)?;
    }
    Ok(())
}

fn check_installed(path: &Path) -> Result<()> {
    if inspect(path, None)?.is_none() {
        return Err("请先运行 clipwipe install".into());
    }
    Ok(())
}

pub(crate) fn install() -> Result<()> {
    platform::require_admin()?;
    let path = platform::binary()?;
    install_at(&path)?;
    println!("已为所有用户安装到 {}", path.display());
    Ok(())
}

fn install_at(path: &Path) -> Result<()> {
    let _lock = setup_lock(path)?;
    let data = fs::read(env::current_exe()?)?;
    platform::prepare_install(path, &data)?;
    install_file(path, &data)?;
    platform::finish_install(path)
}

pub(crate) fn uninstall() -> Result<()> {
    platform::require_admin()?;
    let path = platform::binary()?;
    uninstall_at(&path)?;
    println!("已卸载 ClipWipe 的系统安装。");
    Ok(())
}

fn uninstall_at(path: &Path) -> Result<()> {
    let _lock = setup_lock(path)?;
    let installed = inspect(path, None)?.is_some();
    platform::prepare_uninstall(path, installed)?;
    platform::remove_install(path, installed)?;
    remove_if_present(&record(path))?;
    platform::finish_uninstall(path)
}

pub(crate) fn enable(options: WatchOptions) -> Result<()> {
    let path = platform::binary()?;
    enable_at(&path, options)
}

fn enable_at(path: &Path, options: WatchOptions) -> Result<()> {
    save_autostart(path, options)?;
    let status = crate::background::start(path, options)
        .map_err(|error| format!("登录自启已保存，但当前会话启动监听失败：{error}"))?;
    match status {
        crate::background::StartStatus::Started => println!(
            "登录自启已保存，当前会话已开始后台监听（{} 秒，{}）。",
            options.timeout.as_secs(),
            options.mode.as_str()
        ),
        crate::background::StartStatus::AlreadyRunning => println!(
            "当前会话已有监听实例，继续使用该实例的参数。已保存下次登录设置（{} 秒，{}）。",
            options.timeout.as_secs(),
            options.mode.as_str()
        ),
    }
    Ok(())
}

fn save_autostart(path: &Path, options: WatchOptions) -> Result<()> {
    check_installed(path)?;
    platform::enable(path, options)
}

pub(crate) fn disable() -> Result<()> {
    let path = platform::binary()?;
    platform::disable(&path)?;
    println!("已禁用当前用户登录自启。");
    Ok(())
}

fn setup_lock(binary: &Path) -> Result<fs::File> {
    // Use a system directory so all administrators see the same lock.
    let path = platform::setup_directory(binary)?.join(".clipwipe-setup.lock");
    regular_file(&path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    file.try_lock()
        .map_err(|e| format!("无法取得安装锁（请确认没有其他安装命令正在运行）：{e}"))?;
    Ok(file)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const HEADER: &str = "[Desktop Entry]\n# Managed by ClipWipe\n";

    pub(super) fn binary() -> Result<PathBuf> {
        Ok(PathBuf::from("/usr/local/bin/clipwipe"))
    }

    pub(super) fn require_admin() -> Result<()> {
        // SAFETY: geteuid has no arguments or preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err("系统安装和卸载需要 root 权限，请使用 sudo。".into());
        }
        Ok(())
    }

    pub(super) fn setup_directory(binary: &Path) -> Result<&Path> {
        let directory = binary.parent().ok_or("安装路径缺少父目录")?;
        let ancestors: Vec<_> = directory.ancestors().collect();
        for ancestor in ancestors.into_iter().rev() {
            let metadata = match fs::symlink_metadata(ancestor) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(ancestor)?;
                    fs::set_permissions(ancestor, fs::Permissions::from_mode(0o755))?;
                    fs::symlink_metadata(ancestor)?
                }
                Err(error) => return Err(error.into()),
            };
            let mode = metadata.mode();
            if !metadata.is_dir() || metadata.uid() != 0 || mode & 0o022 != 0 || mode & 0o001 == 0 {
                return Err(format!(
                    "系统安装目录需要由 root 管理且允许所有用户访问：{}",
                    ancestor.display()
                )
                .into());
            }
        }
        Ok(directory)
    }

    fn desktop() -> Result<PathBuf> {
        let config = match env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => absolute_env("HOME")?.join(".config"),
        };
        Ok(config.join("autostart/clipwipe.desktop"))
    }

    fn quote_exec(path: &Path) -> Result<String> {
        let raw = path.to_str().ok_or("自启要求可执行文件路径使用 UTF-8")?;
        if raw.contains('=') || raw.chars().any(char::is_control) {
            return Err("XDG 自启不支持包含等号或控制字符的可执行文件路径".into());
        }
        let mut result = String::from("\"");
        for ch in raw.chars() {
            // Desktop entry string escaping is decoded before Exec argument quoting.
            match ch {
                '\\' => result.push_str("\\\\\\\\"),
                '"' | '$' | '\u{60}' => {
                    result.push_str("\\\\");
                    result.push(ch);
                }
                '%' => result.push_str("%%"),
                _ => result.push(ch),
            }
        }
        result.push('"');
        Ok(result)
    }

    fn own_desktop(path: &Path) -> Result<bool> {
        if !regular_file(path)? {
            return Ok(false);
        }
        if !fs::read_to_string(path)?.starts_with(HEADER) {
            return Err(format!("同名自启文件由其他程序管理：{}", path.display()).into());
        }
        Ok(true)
    }

    pub(super) fn enable(path: &Path, options: WatchOptions) -> Result<()> {
        let file = desktop()?;
        own_desktop(&file)?;
        let exec = quote_exec(path)?;
        let try_exec = path
            .to_str()
            .expect("quote_exec checks UTF-8")
            .replace('\\', "\\\\");
        let content = format!(
            "{HEADER}Type=Application\nName=ClipWipe\nTryExec={try_exec}\nExec={exec} watch --timeout {} --mode {}\nTerminal=false\n",
            options.timeout.as_secs(),
            options.mode.as_str()
        );
        publish(&file, content.as_bytes(), 0o600)
    }
    pub(super) fn disable(_: &Path) -> Result<()> {
        let file = desktop()?;
        if own_desktop(&file)? {
            fs::remove_file(file)?;
        }
        Ok(())
    }
    pub(super) fn prepare_install(_: &Path, _: &[u8]) -> Result<()> {
        Ok(())
    }
    pub(super) fn finish_install(path: &Path) -> Result<()> {
        if !env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .any(|entry| Some(entry.as_path()) == path.parent())
        {
            println!("当前终端的 PATH 缺少 /usr/local/bin，可直接运行 /usr/local/bin/clipwipe。");
        }
        Ok(())
    }
    pub(super) fn prepare_uninstall(_: &Path, _: bool) -> Result<()> {
        Ok(())
    }
    pub(super) fn remove_install(path: &Path, _: bool) -> Result<()> {
        remove_if_present(path)
    }
    pub(super) fn finish_uninstall(_: &Path) -> Result<()> {
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn exec_escapes_both_desktop_entry_and_argument_syntax() {
            assert_eq!(
                quote_exec(Path::new("/a b/10%/$x\"\\")).unwrap(),
                "\"/a b/10%%/\\\\$x\\\\\"\\\\\\\\\""
            );
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, OsString, c_void},
        os::windows::{
            ffi::OsStringExt,
            fs::MetadataExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        },
        process::{Command, Stdio},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, FreeLibrary, HANDLE, HMODULE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        System::{
            Com::COINIT_APARTMENTTHREADED,
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Registry::*,
            SystemInformation::GetSystemDirectoryW,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
        UI::{
            Shell::FOLDERID_ProgramFiles,
            WindowsAndMessaging::{
                HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
            },
        },
    };
    use windows_sys::core::{GUID, HRESULT, PWSTR};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    const LAUNCHER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/clipwipe-startup.exe"));
    const PATH_CREATED: &str = "ClipWipe created system PATH v1";
    const PATH_EXTENDED: &str = "ClipWipe extended system PATH v1";
    type Value = (String, REG_VALUE_TYPE);

    pub(super) fn require_admin() -> Result<()> {
        let mut handle = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut written = 0;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut written,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if elevation.TokenIsElevated == 0 {
            return Err("系统安装和卸载需要管理员权限，请以管理员身份打开终端。".into());
        }
        Ok(())
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }
    fn status(code: u32) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32).into())
        }
    }

    fn read_value(root: HKEY, subkey: &str, name: &str) -> Result<Option<Value>> {
        let (key, name) = (wide(subkey), wide(name));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let mut data = vec![0u16; 256];
        loop {
            let mut size = (data.len() * 2) as u32;
            let mut kind = 0;
            let result = unsafe {
                RegGetValueW(
                    root,
                    key.as_ptr(),
                    name.as_ptr(),
                    flags,
                    &mut kind,
                    data.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if result == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if result == ERROR_MORE_DATA {
                data.resize((size as usize).div_ceil(2).max(data.len() + 1), 0);
                continue;
            }
            status(result)?;
            let units = &data[..size as usize / 2];
            let units = units.strip_suffix(&[0]).unwrap_or(units);
            if units.contains(&0) {
                return Err("注册表字符串包含内嵌空字符".into());
            }
            return Ok(Some((String::from_utf16(units)?, kind)));
        }
    }

    struct OpenKey(HKEY);
    impl Drop for OpenKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn change_value(
        root: HKEY,
        subkey: &str,
        name: &str,
        value: Option<(&str, REG_VALUE_TYPE)>,
    ) -> Result<()> {
        let mut key = ptr::null_mut();
        let subkey = wide(subkey);
        if value.is_some() {
            status(unsafe {
                RegCreateKeyExW(
                    root,
                    subkey.as_ptr(),
                    0,
                    ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    ptr::null(),
                    &mut key,
                    ptr::null_mut(),
                )
            })?;
        } else {
            let result =
                unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
            if result == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            status(result)?;
        }
        let key = OpenKey(key);
        let name = wide(name);
        if let Some((text, kind)) = value {
            let data = wide(text);
            status(unsafe {
                RegSetValueExW(
                    key.0,
                    name.as_ptr(),
                    0,
                    kind,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            })
        } else {
            let result = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
            if result == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            status(result)
        }
    }

    type LibraryProc = unsafe extern "system" fn() -> isize;

    struct SystemLibrary(HMODULE);

    impl SystemLibrary {
        fn load(name: &str) -> Result<Self> {
            let filename = wide(name);
            let module = unsafe {
                LoadLibraryExW(
                    filename.as_ptr(),
                    ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return Err(
                    format!("无法加载系统库 {name}：{}", io::Error::last_os_error()).into(),
                );
            }
            Ok(Self(module))
        }

        fn symbol(&self, name: &CStr) -> Result<LibraryProc> {
            unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }.ok_or_else(|| {
                format!(
                    "无法加载系统函数 {}：{}",
                    name.to_string_lossy(),
                    io::Error::last_os_error()
                )
                .into()
            })
        }
    }

    impl Drop for SystemLibrary {
        fn drop(&mut self) {
            unsafe { FreeLibrary(self.0) };
        }
    }

    pub(super) fn binary() -> Result<PathBuf> {
        type GetKnownFolderPath =
            unsafe extern "system" fn(*const GUID, u32, HANDLE, *mut PWSTR) -> HRESULT;
        type InitializeCom = unsafe extern "system" fn(*const c_void, u32) -> HRESULT;
        type UninitializeCom = unsafe extern "system" fn();
        type FreeComMemory = unsafe extern "system" fn(*const c_void);

        // Keep shell and COM loading on the setup path. Clipboard commands use neither.
        let shell = SystemLibrary::load("shell32.dll")?;
        let ole = SystemLibrary::load("ole32.dll")?;
        // SAFETY: Each export is bound to its Win32 ABI, and both modules remain loaded.
        let (get_folder, initialize, uninitialize, free) = unsafe {
            (
                std::mem::transmute::<LibraryProc, GetKnownFolderPath>(
                    shell.symbol(c"SHGetKnownFolderPath")?,
                ),
                std::mem::transmute::<LibraryProc, InitializeCom>(ole.symbol(c"CoInitializeEx")?),
                std::mem::transmute::<LibraryProc, UninitializeCom>(ole.symbol(c"CoUninitialize")?),
                std::mem::transmute::<LibraryProc, FreeComMemory>(ole.symbol(c"CoTaskMemFree")?),
            )
        };
        let init = unsafe { initialize(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if init < 0 {
            return Err(format!("无法初始化 Windows COM：0x{init:08X}").into());
        }
        let mut raw = ptr::null_mut();
        let status = unsafe { get_folder(&FOLDERID_ProgramFiles, 0, ptr::null_mut(), &mut raw) };
        let result = if status < 0 || raw.is_null() {
            Err(format!("无法定位 Program Files：0x{status:08X}").into())
        } else {
            // SHGetKnownFolderPath returns a NUL-terminated UTF-16 string.
            let length = (0..)
                .take_while(|&offset| unsafe { *raw.add(offset) != 0 })
                .count();
            let path = PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(raw, length)
            }));
            if path.is_absolute() {
                Ok(path.join(r"ClipWipe\clipwipe.exe"))
            } else {
                Err("Program Files 的路径不是绝对路径。".into())
            }
        };
        unsafe {
            free(raw.cast());
            uninitialize();
        }
        result
    }

    fn inspect_directory(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(meta)
                if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 =>
            {
                Ok(true)
            }
            Ok(_) => Err(format!("系统安装需要普通目录：{}", path.display()).into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn setup_directory(binary: &Path) -> Result<&Path> {
        let install_dir = binary.parent().ok_or("安装路径缺少父目录")?;
        let program_files = install_dir.parent().ok_or("找不到 Program Files 目录")?;
        if !inspect_directory(program_files)? {
            return Err("找不到 Program Files 目录。".into());
        }
        inspect_directory(install_dir)?;
        Ok(program_files)
    }
    fn helper(binary: &Path) -> PathBuf {
        binary.with_file_name("clipwipe-startup.exe")
    }
    fn path_record(binary: &Path) -> PathBuf {
        binary.with_file_name(".clipwipe-system-path")
    }
    fn directory(binary: &Path) -> Result<&str> {
        let dir = binary
            .parent()
            .and_then(Path::to_str)
            .ok_or("安装路径需要有效 Unicode 文本")?;
        if dir.contains([';', '%']) {
            return Err("安装路径包含 PATH 不支持的分隔符或变量标记".into());
        }
        Ok(dir)
    }

    fn owned_run(binary: &Path) -> Result<bool> {
        let Some((existing, kind)) = read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")? else {
            return Ok(false);
        };
        let prefix = format!("\"{}\" --timeout ", helper(binary).display());
        let valid = existing
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once(" --mode "))
            .is_some_and(|(seconds, mode)| {
                seconds.parse::<u64>().is_ok_and(|n| n > 0) && matches!(mode, "clear" | "random")
            });
        if !valid || kind != REG_SZ {
            return Err("注册表中同名的 ClipWipe 启动项由其他程序管理".into());
        }
        Ok(true)
    }
    pub(super) fn enable(binary: &Path, options: WatchOptions) -> Result<()> {
        check_installed(&helper(binary))?;
        owned_run(binary)?;
        let command = format!(
            "\"{}\" --timeout {} --mode {}",
            helper(binary).display(),
            options.timeout.as_secs(),
            options.mode.as_str()
        );
        if command.encode_utf16().count() > 259 {
            return Err("启动命令超出 Windows Run 项的 260 字符限制".into());
        }
        change_value(HKEY_CURRENT_USER, RUN, "ClipWipe", Some((&command, REG_SZ)))
    }
    pub(super) fn disable(binary: &Path) -> Result<()> {
        if owned_run(binary)? {
            change_value(HKEY_CURRENT_USER, RUN, "ClipWipe", None)?;
        }
        Ok(())
    }

    fn matching_path(entry: &str, directory: &str) -> bool {
        let trimmed = entry.trim_matches('"').trim_end_matches(['\\', '/']);
        trimmed.replace('/', "\\").eq_ignore_ascii_case(directory)
    }

    fn created_path_value(binary: &Path) -> Result<Option<bool>> {
        let record = path_record(binary);
        if !regular_file(&record)? {
            return Ok(None);
        }
        match fs::read_to_string(&record)?.as_str() {
            PATH_CREATED => Ok(Some(true)),
            PATH_EXTENDED => Ok(Some(false)),
            _ => Err(format!("无法识别 PATH 安装记录：{}", record.display()).into()),
        }
    }

    fn notify_path() {
        #[cfg(test)]
        if env::var_os("CLIPWIPE_HIVE_TEST").is_some() {
            return;
        }
        let environment = wide("Environment");
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                environment.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                250,
                ptr::null_mut(),
            );
        }
    }
    fn without_path_entry(current: &str, directory: &str) -> Option<String> {
        let mut entries: Vec<_> = current.split(';').collect();
        let index = entries
            .iter()
            .rposition(|entry| matching_path(entry, directory))?;
        entries.remove(index);
        Some(entries.join(";"))
    }

    fn with_path_entry(current: &str, directory: &str) -> Result<Option<String>> {
        if current
            .split(';')
            .any(|entry| matching_path(entry, directory))
        {
            return Ok(None);
        }
        let updated = if current.is_empty() {
            directory.to_owned()
        } else {
            format!("{current};{directory}")
        };
        if updated.encode_utf16().count() >= 32767 {
            return Err("系统 PATH 已达到 Windows 长度限制".into());
        }
        Ok(Some(updated))
    }

    fn update_path(binary: &Path, install: bool) -> Result<()> {
        let dir = directory(binary)?;
        let created = created_path_value(binary)?;
        if !install && created.is_none() {
            return Ok(());
        }
        let current = read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?;
        let missing = current.is_none();
        let (current, kind) = current.unwrap_or_else(|| (String::new(), REG_EXPAND_SZ));
        if install {
            let Some(new) = with_path_entry(&current, dir)? else {
                return Ok(());
            };
            if created.is_none() {
                let marker = if missing { PATH_CREATED } else { PATH_EXTENDED };
                publish(&path_record(binary), marker.as_bytes(), SHARED_RECORD)?;
            }
            change_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path", Some((&new, kind)))?;
        } else {
            if let Some(new) = without_path_entry(&current, dir) {
                change_value(
                    HKEY_LOCAL_MACHINE,
                    ENVIRONMENT,
                    "Path",
                    if new.is_empty() && created == Some(true) {
                        None
                    } else {
                        Some((&new, kind))
                    },
                )?;
            }
            remove_if_present(&path_record(binary))?;
        }
        notify_path();
        Ok(())
    }

    fn idle() -> Result<()> {
        if crate::windows::watcher_running()? {
            return Err(
                "请先在监听窗口按 Ctrl+C，或在任务管理器结束当前用户的常驻 clipwipe.exe，再重试。"
                    .into(),
            );
        }
        Ok(())
    }
    pub(super) fn prepare_install(path: &Path, bytes: &[u8]) -> Result<()> {
        let new = digest(bytes);
        if inspect(path, Some(&new))?.is_some_and(|old| old != new) {
            idle()?;
        }
        inspect(&helper(path), Some(&digest(LAUNCHER)))?;
        created_path_value(path)?;
        let directory = directory(path)?;
        let current = read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?;
        let _ = with_path_entry(current.as_ref().map_or("", |(value, _)| value), directory)?;
        Ok(())
    }
    pub(super) fn finish_install(path: &Path) -> Result<()> {
        install_file(&helper(path), LAUNCHER)?;
        update_path(path, true)?;
        println!("请从开始菜单重新打开终端，然后运行 clipwipe --version。");
        Ok(())
    }
    pub(super) fn prepare_uninstall(path: &Path, installed: bool) -> Result<()> {
        inspect(&helper(path), None)?;
        if created_path_value(path)?.is_some() {
            read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?;
        }
        if installed {
            idle()?;
        }
        Ok(())
    }

    fn retire_current(path: &Path) -> Result<()> {
        let parent = path.parent().ok_or("安装目录无效")?;
        let retired = temporary_path(parent.parent().ok_or("安装目录缺少上级目录")?)?;
        let mut directory = vec![0; 260];
        let length = unsafe { GetSystemDirectoryW(directory.as_mut_ptr(), directory.len() as u32) };
        if length == 0 || length as usize >= directory.len() {
            return Err("无法定位 Windows 系统目录。".into());
        }
        let powershell = PathBuf::from(OsString::from_wide(&directory[..length as usize]))
            .join(r"WindowsPowerShell\v1.0\powershell.exe");
        let script = r#"
            try {
                $owner = [Diagnostics.Process]::GetProcessById([int]$env:CLIPWIPE_PARENT)
                $null = $owner.WaitForExit(60000)
                $owner.Dispose()
            } catch { }
            for ($attempt = 0; $attempt -lt 100; $attempt++) {
                try { [IO.File]::Delete($env:CLIPWIPE_RETIRED); break }
                catch { [Threading.Thread]::Sleep(100) }
            }
        "#;
        // Start cleanup before moving the mapped image, so an interruption cannot skip cleanup.
        // Only the retired name is removed, even if the user reinstalls immediately.
        Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("CLIPWIPE_RETIRED", &retired)
            .env("CLIPWIPE_PARENT", std::process::id().to_string())
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        fs::rename(path, &retired)?;
        Ok(())
    }
    pub(super) fn remove_install(path: &Path, installed: bool) -> Result<()> {
        if installed && let Err(err) = fs::remove_file(path) {
            if fs::canonicalize(env::current_exe()?)? == fs::canonicalize(path)? {
                retire_current(path)?;
            } else {
                return Err(
                    format!("无法删除安装文件，请结束使用该文件的进程再重试：{err}").into(),
                );
            }
        }
        remove_if_present(&helper(path))?;
        remove_if_present(&record(&helper(path)))?;
        update_path(path, false)
    }
    pub(super) fn finish_uninstall(path: &Path) -> Result<()> {
        match fs::remove_dir(path.parent().expect("fixed install location")) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::ffi::OsStrExt;

        // The external fixture owns the window station, test directory and watcher cleanup.
        #[test]
        #[ignore = "requires a disposable window station and staged release executable"]
        fn enable_immediately_in_private_station() -> Result<()> {
            use windows_sys::Win32::System::StationsAndDesktops::{
                GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
            };
            let mut flags = USEROBJECTFLAGS::default();
            let ok = unsafe {
                GetUserObjectInformationW(
                    GetProcessWindowStation(),
                    UOI_FLAGS,
                    (&mut flags as *mut USEROBJECTFLAGS).cast(),
                    size_of::<USEROBJECTFLAGS>() as u32,
                    ptr::null_mut(),
                )
            };
            assert!(
                ok != 0 && flags.dwFlags & 1 == 0,
                "a noninteractive window station is required"
            );
            let root = absolute_env("CLIPWIPE_ACTIVATION_FIXTURE")?;
            let binary = root.join("clipwipe.exe");
            install_file(&binary, &fs::read(&binary)?)?;
            install_file(&helper(&binary), LAUNCHER)?;
            let hive: Vec<_> = root
                .join("autostart-test.dat")
                .as_os_str()
                .encode_wide()
                .chain([0])
                .collect();
            let mut key = ptr::null_mut();
            status(unsafe {
                RegLoadAppKeyW(
                    hive.as_ptr(),
                    &mut key,
                    KEY_ALL_ACCESS,
                    REG_PROCESS_APPKEY,
                    0,
                )
            })?;
            let key = OpenKey(key);
            status(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, key.0) })?;
            let opts = WatchOptions {
                timeout: std::time::Duration::from_secs(1),
                mode: crate::Mode::Random,
            };
            enable_at(&binary, opts)?;
            enable_at(&binary, opts)?;
            assert!(owned_run(&binary)?);
            status(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, ptr::null_mut()) })?;
            Ok(())
        }

        #[test]
        fn retired_running_image_preserves_an_immediate_reinstall() -> Result<()> {
            let executable = env::current_exe()?;
            if let Some(root) = env::var_os("CLIPWIPE_SELF_REMOVE_TEST") {
                let target = PathBuf::from(root).join("app").join("clipwipe.exe");
                assert_eq!(target.canonicalize()?, executable.canonicalize()?);
                retire_current(&executable)?;
                fs::write(target, b"reinstalled")?;
                return Ok(());
            }
            let root = temporary_path(&env::temp_dir())?;
            let app = root.join("app");
            fs::create_dir(&root)?;
            fs::create_dir(&app)?;
            let binary = app.join("clipwipe.exe");
            fs::copy(executable, &binary)?;
            let result = Command::new(&binary)
                .args(["--exact", "install::platform::tests::retired_running_image_preserves_an_immediate_reinstall", "--nocapture"])
                .env("CLIPWIPE_SELF_REMOVE_TEST", &root)
                .output()?;
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while fs::read_dir(&root)?.count() > 1 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert_eq!(fs::read_dir(&root)?.count(), 1, "retired image remains");
            assert_eq!(fs::read(&binary)?, b"reinstalled");
            fs::remove_file(binary)?;
            fs::remove_dir(app)?;
            fs::remove_dir(root)?;
            Ok(())
        }

        #[test]
        fn installs_and_configures_login_in_private_registry_hive() -> Result<()> {
            // Run the registry redirection in an isolated process. RegOverridePredefKey
            // affects that process only, and RegLoadAppKey creates a private disk hive.
            if env::var_os("CLIPWIPE_HIVE_TEST").is_none() {
                let temp = temporary_path(&env::temp_dir())?;
                fs::create_dir(&temp)?;
                let result = Command::new(env::current_exe()?)
                    .args(["--exact", "install::platform::tests::installs_and_configures_login_in_private_registry_hive", "--nocapture"])
                    .env("CLIPWIPE_HIVE_TEST", &temp)
                    .output()?;
                fs::remove_dir_all(temp)?;
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
                return Ok(());
            }
            let temp = absolute_env("CLIPWIPE_HIVE_TEST")?;
            let binary = temp.join(r"ClipWipe\clipwipe.exe");
            assert_ne!(super::binary()?, binary);
            let open_hive = |name| -> Result<OpenKey> {
                let hive: Vec<u16> = temp
                    .join(name)
                    .as_os_str()
                    .encode_wide()
                    .chain([0])
                    .collect();
                let mut key = ptr::null_mut();
                status(unsafe {
                    RegLoadAppKeyW(
                        hive.as_ptr(),
                        &mut key,
                        KEY_ALL_ACCESS,
                        REG_PROCESS_APPKEY,
                        0,
                    )
                })?;
                Ok(OpenKey(key))
            };
            let machine = open_hive("machine.dat")?;
            let user = open_hive("user.dat")?;
            status(unsafe { RegOverridePredefKey(HKEY_LOCAL_MACHINE, machine.0) })?;
            status(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, user.0) })?;
            let original = r"%SystemRoot%\System32;;C:\tools";
            let user_path = r"%USERPROFILE%\bin";
            let full_path = "x".repeat(32760);
            change_value(
                HKEY_LOCAL_MACHINE,
                ENVIRONMENT,
                "Path",
                Some((&full_path, REG_EXPAND_SZ)),
            )?;
            assert!(install_at(&binary).is_err());
            assert!(!binary.exists());
            assert!(!helper(&binary).exists());
            assert!(!record(&binary).exists());
            change_value(
                HKEY_LOCAL_MACHINE,
                ENVIRONMENT,
                "Path",
                Some((original, REG_EXPAND_SZ)),
            )?;
            change_value(
                HKEY_CURRENT_USER,
                "Environment",
                "Path",
                Some((user_path, REG_EXPAND_SZ)),
            )?;
            install_at(&binary)?;
            install_at(&binary)?;
            let expected = format!("{original};{}", directory(&binary)?);
            assert_eq!(
                read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?,
                Some((expected, REG_EXPAND_SZ))
            );
            assert!(read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?.is_none());
            assert_eq!(
                read_value(HKEY_CURRENT_USER, "Environment", "Path")?,
                Some((user_path.to_owned(), REG_EXPAND_SZ))
            );
            save_autostart(
                &binary,
                WatchOptions {
                    timeout: std::time::Duration::from_secs(77),
                    mode: crate::Mode::Random,
                },
            )?;
            let (command, kind) = read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?.unwrap();
            assert_eq!(
                command,
                format!(
                    "\"{}\" --timeout 77 --mode random",
                    helper(&binary).display()
                )
            );
            assert_eq!(kind, REG_SZ);
            assert!(read_value(HKEY_LOCAL_MACHINE, RUN, "ClipWipe")?.is_none());
            disable(&binary)?;
            assert!(read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?.is_none());
            change_value(
                HKEY_CURRENT_USER,
                RUN,
                "ClipWipe",
                Some(("another application", REG_SZ)),
            )?;
            assert!(disable(&binary).is_err());
            change_value(HKEY_CURRENT_USER, RUN, "ClipWipe", None)?;
            save_autostart(&binary, WatchOptions::default())?;
            let user_startup = read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?;
            // Remove the isolated files directly if a real watcher is already active.
            // Its named mutex belongs to the interactive session, outside the hive.
            if crate::windows::watcher_running()? {
                remove_install(&binary, true)?;
                remove_if_present(&record(&binary))?;
                finish_uninstall(&binary)?;
            } else {
                uninstall_at(&binary)?;
            }
            assert_eq!(
                read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?,
                user_startup
            );
            disable(&binary)?;
            assert!(read_value(HKEY_CURRENT_USER, RUN, "ClipWipe")?.is_none());
            assert_eq!(
                read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?,
                Some((original.to_owned(), REG_EXPAND_SZ))
            );
            assert!(!binary.exists());
            for previous in [
                None,
                Some(("", REG_SZ)),
                Some((directory(&binary)?, REG_SZ)),
            ] {
                change_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path", previous)?;
                update_path(&binary, true)?;
                update_path(&binary, false)?;
                assert_eq!(
                    read_value(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?,
                    previous.map(|(s, kind)| (s.to_owned(), kind))
                );
            }
            assert!(!path_record(&binary).exists());
            status(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, ptr::null_mut()) })?;
            status(unsafe { RegOverridePredefKey(HKEY_LOCAL_MACHINE, ptr::null_mut()) })?;
            Ok(())
        }
        #[test]
        fn removing_a_path_entry_preserves_other_entries_and_empty_segments() {
            let dir = r"C:\User\ClipWipe";
            assert_eq!(
                without_path_entry(r"test;;C:\USER\CLIPWIPE\;%OLD%", dir).as_deref(),
                Some("test;;%OLD%")
            );
            assert!(without_path_entry(r"C:\User\ClipWipe-extra", dir).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installation_checks_ownership_and_recovers_from_interrupted_publish() -> Result<()> {
        let folder = temporary_path(&env::temp_dir())?;
        fs::create_dir(&folder)?;
        let path = folder.join("clipwipe");
        publish(&path, b"unrelated", 0o600)?;
        assert!(install_file(&path, b"ours").is_err());
        assert!(inspect(&path, None).is_err());
        fs::remove_file(&path)?;
        install_file(&path, b"ours")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        install_file(&path, b"ours")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)?.permissions().mode() & 0o777,
                EXECUTABLE
            );
            assert_eq!(
                fs::metadata(record(&path))?.permissions().mode() & 0o777,
                SHARED_RECORD
            );
        }
        let old = digest(b"ours");
        let new = digest(b"updated");
        publish(
            &record(&path),
            format!("{RECORD_HEADER}\n{old}\n{new}\n").as_bytes(),
            SHARED_RECORD,
        )?;
        install_file(&path, b"updated")?;
        assert_eq!(inspect(&path, None)?, Some(new));
        publish(&path, b"changed externally", 0o600)?;
        assert!(inspect(&path, None).is_err());
        fs::remove_file(&path)?;
        fs::remove_file(record(&path))?;
        fs::remove_dir(folder)?;
        Ok(())
    }
}
