//! Start-at-login: the persisted setting (default on) and per-OS registration.
//! macOS uses a LaunchAgent plist; Windows uses the HKCU Run registry key.

use std::path::{Path, PathBuf};

pub const LABEL: &str = "com.iceman0d.clipsync";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Persisted settings. Plain `key=value` lines; unknown or missing keys keep defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { autostart: true }
    }
}

impl Settings {
    pub fn to_text(&self) -> String {
        format!("autostart={}\n", self.autostart)
    }

    pub fn from_text(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            if let Some(("autostart", v)) = line.trim().split_once('=') {
                match v.trim() {
                    "true" => s.autostart = true,
                    "false" => s.autostart = false,
                    _ => {}
                }
            }
        }
        s
    }

    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path).map(|t| Settings::from_text(&t)).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_text())
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// LaunchAgent plist that runs `<exe> app` at login.
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{LABEL}</string>\n\
  <key>ProgramArguments</key>\n  <array><string>{}</string><string>app</string></array>\n\
  <key>RunAtLoad</key><true/>\n\
</dict>\n</plist>\n",
        xml_escape(&exe.to_string_lossy())
    )
}

pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// Arguments for `reg.exe` to register (`Some(exe)`) or remove (`None`) the Run entry.
pub fn reg_args(exe: Option<&Path>) -> Vec<String> {
    match exe {
        Some(exe) => vec![
            "add".into(),
            RUN_KEY.into(),
            "/v".into(),
            "ClipSync".into(),
            "/t".into(),
            "REG_SZ".into(),
            "/d".into(),
            format!("\"{}\" app", exe.display()),
            "/f".into(),
        ],
        None => vec!["delete".into(), RUN_KEY.into(), "/v".into(), "ClipSync".into(), "/f".into()],
    }
}

/// Make the OS login registration match `enabled`.
pub fn apply(enabled: bool) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    apply_os(enabled, &exe)
}

#[cfg(target_os = "macos")]
fn apply_os(enabled: bool, exe: &Path) -> std::io::Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| std::io::Error::other("HOME not set"))?;
    let path = launch_agent_path(&home);
    if enabled {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, launch_agent_plist(exe))
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
fn apply_os(enabled: bool, exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(reg_args(enabled.then_some(exe)))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()?;
    // Deleting a value that does not exist is fine.
    if out.status.success() || !enabled {
        Ok(())
    } else {
        Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn apply_os(_enabled: bool, _exe: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_on() {
        assert!(Settings::default().autostart);
        assert!(Settings::from_text("").autostart);
        assert!(Settings::from_text("autostart=garbage\n").autostart);
    }

    #[test]
    fn round_trips_off() {
        let off = Settings { autostart: false };
        assert_eq!(Settings::from_text(&off.to_text()), off);
        assert_eq!(off.to_text(), "autostart=false\n");
    }

    #[test]
    fn persists_to_disk() {
        let dir = std::env::temp_dir().join(format!("clipsync-as-{}", std::process::id()));
        let path = dir.join("sub").join("settings.txt");
        assert!(Settings::load(&path).autostart, "missing file means default on");
        Settings { autostart: false }.save(&path).unwrap();
        assert!(!Settings::load(&path).autostart);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plist_runs_app_at_load() {
        let p = launch_agent_plist(Path::new("/Applications/Clip & Sync/clipsync"));
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
        assert!(p.contains("<string>/Applications/Clip &amp; Sync/clipsync</string><string>app</string>"));
        assert!(p.contains(LABEL));
        assert_eq!(
            launch_agent_path(Path::new("/Users/a")),
            PathBuf::from("/Users/a/Library/LaunchAgents/com.iceman0d.clipsync.plist")
        );
    }

    #[test]
    fn reg_args_add_and_delete() {
        let add = reg_args(Some(Path::new(r"C:\Program Files\ClipSync\clipsync.exe")));
        assert_eq!(add[0], "add");
        assert!(add.contains(&r#""C:\Program Files\ClipSync\clipsync.exe" app"#.to_string()));
        assert_eq!(reg_args(None)[0], "delete");
    }
}
