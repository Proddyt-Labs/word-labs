//! Windows: ask GitHub (with the system's `curl.exe`), ask the user, download, hand over to the
//! installer once the app has quit.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Mutex, OnceLock};

use crate::version::newer;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// The largest answer or file accepted (bytes).
const MAX_ANSWER: u64 = 4 << 20;
const MAX_FILE: u64 = 512 << 20;

/// Waits for the app (`-AppPid`) to quit, installs the update and reopens the app.
const HELPER: &str = r#"param([int]$AppPid, [string]$Kind, [string]$File, [string]$Dir, [string]$Exe)
$ErrorActionPreference = 'Stop'
$log = Join-Path $env:TEMP 'proddyt-switch-update.log'
try {
    Wait-Process -Id $AppPid -ErrorAction SilentlyContinue
    if ($Kind -eq 'installed') {
        $p = Start-Process -FilePath $File -ArgumentList '/SILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "installer exit $($p.ExitCode)" }
    } else {
        $x = Join-Path $env:TEMP ('proddyt-switch-' + [guid]::NewGuid())
        Expand-Archive -Path $File -DestinationPath $x
        $src = Get-ChildItem $x -Directory | Select-Object -First 1
        Copy-Item (Join-Path $src.FullName '*') $Dir -Recurse -Force
        Remove-Item $x -Recurse -Force
    }
    Add-Content $log "$(Get-Date -Format s) $Exe updated ($Kind)"
} catch {
    Add-Content $log "$(Get-Date -Format s) $Exe failed: $($_.Exception.Message)"
} finally {
    Remove-Item $File -Force -ErrorAction SilentlyContinue
    Start-Process -FilePath $Exe
}
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Install {
    Portable,
    Installed,
}

#[derive(Clone)]
struct Release {
    tag: String,
    /// The download for this copy (installer or portable archive).
    file: String,
}

enum Phase {
    /// Not a release copy, turned off, or finished: nothing more happens this run.
    Off,
    Start,
    Checking(Receiver<Result<Option<Release>, String>>),
    Ask(Release),
    Downloading(Release, Receiver<Result<PathBuf, String>>),
    Ready(Release, PathBuf),
    Failed(String),
}

struct State {
    repo: &'static str,
    name: &'static str,
    phase: Phase,
    current: String,
    install: Install,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Where the user's choices are kept (`%APPDATA%\Proddyt Switch\updates-<repo>.json`).
fn settings_file(repo: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Proddyt Switch").join(format!("updates-{repo}.json")))
}

fn settings(repo: &str) -> serde_json::Value {
    settings_file(repo).and_then(|f| std::fs::read_to_string(f).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn skip(repo: &str, tag: &str) {
    let mut s = settings(repo);
    if !s.is_object() {
        s = serde_json::json!({});
    }
    s["skipped"] = serde_json::Value::String(tag.to_string());
    if let Some(f) = settings_file(repo) {
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(f, s.to_string());
    }
}

/// This copy's release tag and how it was installed, or `None` for a build from source.
fn installed() -> Option<(String, Install, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let tag = std::fs::read_to_string(dir.join("VERSION")).ok()?.trim().to_string();
    if tag.is_empty() || tag.len() > 64 {
        return None;
    }
    let install = if dir.join("unins000.exe").exists() { Install::Installed } else { Install::Portable };
    Some((tag, install, exe))
}

fn curl() -> Command {
    let mut c = Command::new("curl.exe");
    c.creation_flags(CREATE_NO_WINDOW);
    c.args(["-sSfL", "--proto", "=https", "-H", "User-Agent: proddyt-switch-updater"]);
    c
}

/// `url` continues `prefix` with a plain path (release answers are untrusted).
fn under(url: &str, prefix: &str) -> bool {
    url.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '?' | '#' | '\\' | '%')) && !rest.contains("..")
    })
}

/// The newest release newer than `current` that wasn't skipped, with its file for this copy.
fn check(repo: &str, current: &str, install: Install) -> Result<Option<Release>, String> {
    let out = curl()
        .args(["--max-time", "20", "--max-filesize", &MAX_ANSWER.to_string(), "-H", "Accept: application/vnd.github+json"])
        .arg(format!("https://api.github.com/repos/Proddyt-Labs/{repo}/releases?per_page=10"))
        .output()
        .map_err(|e| format!("curl ({e})"))?;
    if !out.status.success() {
        return Err(format!("couldn't reach GitHub ({})", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("unreadable answer ({e})"))?;
    let list = list.as_array().ok_or("unreadable answer")?;
    let releases: Vec<&serde_json::Value> = list.iter().filter(|r| !r["draft"].as_bool().unwrap_or(false)).collect();
    let tags: Vec<&str> = releases.iter().filter_map(|r| r["tag_name"].as_str()).filter(|t| !t.is_empty() && t.len() <= 64).collect();
    let Some(&newest) = newer(&tags, current).first() else { return Ok(None) };
    if settings(repo)["skipped"].as_str() == Some(newest) {
        return Ok(None);
    }
    let suffix = match install {
        Install::Installed => "-windows-x64-setup.exe",
        Install::Portable => "-windows-x64-portable.zip",
    };
    let prefix = format!("https://github.com/Proddyt-Labs/{repo}/releases/download/");
    let file = releases.iter().find(|r| r["tag_name"].as_str() == Some(newest)).and_then(|r| r["assets"].as_array()).and_then(|a| {
        a.iter().filter_map(|f| f["browser_download_url"].as_str()).find(|u| u.ends_with(suffix) && under(u, &prefix)).map(str::to_string)
    });
    Ok(file.map(|file| Release { tag: newest.to_string(), file }))
}

fn download(release: &Release) -> Result<PathBuf, String> {
    let work = std::env::temp_dir().join("proddyt-switch-update");
    std::fs::create_dir_all(&work).map_err(|e| format!("can't prepare the download ({e})"))?;
    let name = release.file.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("update.bin");
    let to = work.join(name);
    let out = curl()
        .args(["--max-time", "1800", "--max-filesize", &MAX_FILE.to_string(), "-o"])
        .arg(&to)
        .arg(&release.file)
        .output()
        .map_err(|e| format!("curl ({e})"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&to);
        return Err(format!("the download failed ({})", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(to)
}

/// Starts the helper that waits for this process to quit and installs `file`.
fn hand_over(file: &Path, install: Install) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find the program ({e})"))?;
    let dir = exe.parent().ok_or("can't find the program's folder")?.to_path_buf();
    let helper = file.with_file_name("update.ps1");
    std::fs::write(&helper, HELPER).map_err(|e| format!("can't prepare the update ({e})"))?;
    let kind = if install == Install::Installed { "installed" } else { "portable" };
    Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
        .arg(&helper)
        .args(["-AppPid", &std::process::id().to_string(), "-Kind", kind, "-File"])
        .arg(file)
        .arg("-Dir")
        .arg(&dir)
        .arg("-Exe")
        .arg(&exe)
        .spawn()
        .map_err(|e| format!("can't start the updater ({e})"))?;
    Ok(())
}

fn short(tag: &str) -> &str {
    tag.trim_start_matches(['v', 'V'])
}

pub fn frame(ctx: &egui::Context, repo: &'static str, name: &'static str) {
    let state = STATE.get_or_init(|| {
        let (phase, current, install) = match installed() {
            Some((tag, install, _)) if settings(repo)["at_start"].as_bool() != Some(false) => (Phase::Start, tag, install),
            _ => (Phase::Off, String::new(), Install::Portable),
        };
        Mutex::new(State { repo, name, phase, current, install })
    });
    let Ok(mut s) = state.lock() else { return };
    let s = &mut *s;
    match &s.phase {
        Phase::Off => {}
        Phase::Start => {
            let (tx, rx) = channel();
            let (repo, current, install, ctx2) = (s.repo, s.current.clone(), s.install, ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(check(repo, &current, install));
                ctx2.request_repaint();
            });
            s.phase = Phase::Checking(rx);
        }
        Phase::Checking(rx) => match rx.try_recv() {
            Ok(Ok(Some(r))) => s.phase = Phase::Ask(r),
            Ok(_) | Err(TryRecvError::Disconnected) => s.phase = Phase::Off,
            Err(TryRecvError::Empty) => {}
        },
        Phase::Ask(r) => {
            let r = r.clone();
            let (mut update, mut later, mut skipped) = (false, false, false);
            let modal = egui::Modal::new(egui::Id::new("labs-updater-ask")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Update available");
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("{} {} is available.", s.name, short(&r.tag))).strong());
                ui.label(format!("You have {}. Update now?", short(&s.current)));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    update = ui.button("Update").clicked();
                    later = ui.button("Not now").clicked();
                    skipped = ui.button("Skip this version").clicked();
                });
            });
            later |= modal.should_close();
            if skipped {
                skip(s.repo, &r.tag);
            }
            if update {
                let (tx, rx) = channel();
                let (r2, ctx2) = (r.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(download(&r2));
                    ctx2.request_repaint();
                });
                s.phase = Phase::Downloading(r, rx);
            } else if later || skipped {
                s.phase = Phase::Off;
            }
        }
        Phase::Downloading(r, rx) => {
            let r = r.clone();
            match rx.try_recv() {
                Ok(Ok(file)) => s.phase = Phase::Ready(r, file),
                Ok(Err(e)) => s.phase = Phase::Failed(e),
                Err(TryRecvError::Disconnected) => s.phase = Phase::Failed("the download stopped".into()),
                Err(TryRecvError::Empty) => {
                    egui::Area::new(egui::Id::new("labs-updater-downloading"))
                        .order(egui::Order::Foreground)
                        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 56.0))
                        .show(ctx, |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(format!("Downloading {} {}…", s.name, short(&r.tag)));
                                });
                            });
                        });
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                }
            }
        }
        Phase::Ready(r, file) => {
            let (r, file) = (r.clone(), file.clone());
            let (mut go, mut later) = (false, false);
            let modal = egui::Modal::new(egui::Id::new("labs-updater-ready")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading(format!("{} {} is ready", s.name, short(&r.tag)));
                ui.add_space(6.0);
                ui.label(format!("Save your work: {} closes, installs the update and opens again.", s.name));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    go = ui.button("Close and update").clicked();
                    later = ui.button("Later").clicked();
                });
            });
            later |= modal.should_close();
            if go {
                match hand_over(&file, s.install) {
                    Ok(()) => {
                        s.phase = Phase::Off;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => s.phase = Phase::Failed(e),
                }
            } else if later {
                let _ = std::fs::remove_file(&file);
                s.phase = Phase::Off;
            }
        }
        Phase::Failed(e) => {
            let e = e.clone();
            let mut ok = false;
            let modal = egui::Modal::new(egui::Id::new("labs-updater-failed")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Couldn't update");
                ui.label(&e);
                ui.label(format!("The releases are at github.com/Proddyt-Labs/{}/releases.", s.repo));
                ok = ui.button("OK").clicked();
            });
            if ok || modal.should_close() {
                s.phase = Phase::Off;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_links_under_the_prefix_pass() {
        let p = "https://github.com/Proddyt-Labs/photo-labs/releases/download/";
        assert!(under(&format!("{p}v1/photocraft-windows-x64-setup.exe"), p));
        for bad in ["https://example.com/x.exe", "https://github.com/Proddyt-Labs/photo-labs/releases/downloadx/a", "", "v1/a b"] {
            assert!(!under(bad, p), "{bad}");
        }
        assert!(!under(&format!("{p}v1/%2e%2e/x"), p));
        assert!(!under(&format!("{p}../x"), p));
    }
}
