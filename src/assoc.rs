//! Регистрира проектните файлове `.ppdl` към програмата за текущия потребител (HKCU, без администраторски права):
//! същата икона като на PP_Downloader.exe и отваряне с двоен клик. Пише в регистъра само ако нещо се е
//! променило (напр. exe-то е преместено), за да не опреснява иконите в Explorer при всяко пускане.

#[cfg(windows)]
pub fn register() {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    const PROG_ID: &str = "PPDownloader.Project";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Задава стойността по подразбиране на ключа; връща true, ако е била различна.
    fn set_default(path: &str, value: &str) -> bool {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            let p = wide(path);
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                p.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            ) != 0
            {
                return false;
            }
            let mut buf = [0u16; 1024];
            let mut len = (buf.len() * 2) as u32;
            let mut kind = 0;
            let current = if RegQueryValueExW(key, std::ptr::null(), std::ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut len) == 0
                && kind == REG_SZ
            {
                String::from_utf16_lossy(&buf[..(len as usize / 2)]).trim_end_matches('\0').to_string()
            } else {
                String::new()
            };
            let changed = current != value;
            if changed {
                let v = wide(value);
                RegSetValueExW(key, std::ptr::null(), 0, REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32);
            }
            RegCloseKey(key);
            changed
        }
    }

    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.display().to_string();
    let classes = r"Software\Classes";
    let mut changed = false;
    changed |= set_default(&format!(r"{classes}\.{}", crate::project::EXT), PROG_ID);
    changed |= set_default(&format!(r"{classes}\{PROG_ID}"), "PP Downloader проект");
    changed |= set_default(&format!(r"{classes}\{PROG_ID}\DefaultIcon"), &format!("\"{exe}\",0"));
    changed |= set_default(&format!(r"{classes}\{PROG_ID}\shell\open\command"), &format!("\"{exe}\" \"%1\""));
    if changed {
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as _, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    }
}

#[cfg(not(windows))]
pub fn register() {}
