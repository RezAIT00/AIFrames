//! File-association registration (`<app> --register` / `--unregister`).
//!
//! Registers an app per-user (HKCU, no admin needed) as a handler for its file types and as a
//! "Default Apps" capable application. Windows does not allow programs to silently make themselves
//! the default (the UserChoice key is hash-protected), so after registering we can open the Settings
//! page where the user confirms the app as the default with one click.

/// Describes an app's shell registration.
#[derive(Clone, Copy, Debug)]
pub struct AppRegistration {
    /// Display name, also used for `Software\<name>` and RegisteredApplications ("AIFrames").
    pub name: &'static str,
    /// Executable file name ("aiframes.exe").
    pub exe_name: &'static str,
    /// ProgID ("AIFrames.Project").
    pub prog_id: &'static str,
    /// File type description ("AIFrames Project").
    pub type_name: &'static str,
    /// Shown in Default Apps.
    pub description: &'static str,
    /// Extensions with leading dot.
    pub extensions: &'static [&'static str],
}

#[cfg(windows)]
mod imp {
    use super::AppRegistration;
    use winreg::enums::*;
    use winreg::{RegKey, RegValue};

    fn e(err: std::io::Error) -> String {
        err.to_string()
    }

    fn set(hkcu: &RegKey, path: &str, name: &str, value: &str) -> Result<(), String> {
        let (k, _) = hkcu.create_subkey(path).map_err(e)?;
        k.set_value(name, &value).map_err(e)
    }

    pub fn register(app: &AppRegistration, open_settings: bool) -> Result<String, String> {
        let exe = std::env::current_exe().map_err(e)?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let exe_s = exe.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let cmd = format!("\"{exe_s}\" \"%1\"");
        let icon = format!("\"{exe_s}\",0");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let caps = format!(r"Software\{}\Capabilities", app.name);

        let prog = format!(r"Software\Classes\{}", app.prog_id);
        set(&hkcu, &prog, "", app.type_name)?;
        set(&hkcu, &prog, "FriendlyTypeName", app.type_name)?;
        set(&hkcu, &format!(r"{prog}\DefaultIcon"), "", &icon)?;
        set(&hkcu, &format!(r"{prog}\shell\open\command"), "", &cmd)?;

        // "Open with" entries for each extension (does not steal the current default).
        for ext in app.extensions {
            let (k, _) = hkcu.create_subkey(format!(r"Software\Classes\{ext}\OpenWithProgids")).map_err(e)?;
            k.set_raw_value(app.prog_id, &RegValue { vtype: REG_NONE, bytes: Vec::new().into() }).map_err(e)?;
        }

        let reg_app = format!(r"Software\Classes\Applications\{}", app.exe_name);
        set(&hkcu, &reg_app, "FriendlyAppName", app.name)?;
        set(&hkcu, &format!(r"{reg_app}\DefaultIcon"), "", &icon)?;
        set(&hkcu, &format!(r"{reg_app}\shell\open\command"), "", &cmd)?;
        for ext in app.extensions {
            set(&hkcu, &format!(r"{reg_app}\SupportedTypes"), ext, "")?;
        }

        set(&hkcu, &caps, "ApplicationName", app.name)?;
        set(&hkcu, &caps, "ApplicationDescription", app.description)?;
        set(&hkcu, &caps, "ApplicationIcon", &icon)?;
        for ext in app.extensions {
            set(&hkcu, &format!(r"{caps}\FileAssociations"), ext, app.prog_id)?;
        }
        set(&hkcu, &format!(r"Software\{}", app.name), "InstallPath", &exe_s)?;
        set(&hkcu, r"Software\RegisteredApplications", app.name, &caps)?;

        notify_shell();
        if open_settings {
            let _ = std::process::Command::new("explorer.exe").arg(format!("ms-settings:defaultapps?registeredAppUser={}", app.name)).spawn();
        }
        Ok(exe_s)
    }

    pub fn unregister(app: &AppRegistration) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{}", app.prog_id));
        let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\Applications\{}", app.exe_name));
        let _ = hkcu.delete_subkey_all(format!(r"Software\{}", app.name));
        for ext in app.extensions {
            if let Ok(k) = hkcu.open_subkey_with_flags(format!(r"Software\Classes\{ext}\OpenWithProgids"), KEY_SET_VALUE) {
                let _ = k.delete_value(app.prog_id);
            }
        }
        if let Ok(k) = hkcu.open_subkey_with_flags(r"Software\RegisteredApplications", KEY_SET_VALUE) {
            let _ = k.delete_value(app.name);
        }
        notify_shell();
        Ok(())
    }

    fn notify_shell() {
        #[link(name = "shell32")]
        extern "system" {
            fn SHChangeNotify(event: i32, flags: u32, item1: *const core::ffi::c_void, item2: *const core::ffi::c_void);
        }
        const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
        const SHCNF_IDLIST: u32 = 0;
        // SAFETY: documented Win32 call with null item pointers for SHCNE_ASSOCCHANGED.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    }
}

/// Register file associations for the current user. Returns the registered executable path.
/// Opens Settings > Default apps unless `--quiet` is on the command line.
#[cfg(windows)]
pub fn register(app: &AppRegistration) -> Result<String, String> {
    let quiet = std::env::args().any(|a| a == "--quiet");
    imp::register(app, !quiet)
}

#[cfg(windows)]
pub fn unregister(app: &AppRegistration) -> Result<(), String> {
    imp::unregister(app)
}

#[cfg(not(windows))]
pub fn register(_app: &AppRegistration) -> Result<String, String> {
    Err("file association registration is only implemented on Windows".into())
}

#[cfg(not(windows))]
pub fn unregister(_app: &AppRegistration) -> Result<(), String> {
    Err("file association registration is only implemented on Windows".into())
}
